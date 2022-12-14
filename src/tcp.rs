use crate::{
    packet::TCPPacket,
    socket::{SockID, Socket, TcpStatus},
    tcpflags,
};
use anyhow::{bail, Context, Result};
use local_ip_address;
use pnet::{
    packet::{ip::IpNextHeaderProtocols, tcp::TcpPacket, Packet},
    transport::{self, TransportChannelType},
};
use rand::{rngs::ThreadRng, Rng};
use std::{
    cmp,
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    ops::Range,
    sync::{Arc, Condvar, Mutex, RwLock, RwLockWriteGuard},
    thread,
    time::{Duration, SystemTime},
};

const MAX_TRANSMITTION: u8 = 5;
const MSS: usize = 1460;
const PORT_RANGE: Range<u16> = 40000..60000;
const RETRANSMITTION_TIMEOUT: u64 = 3;
const UNDETERMINED_IP_ADDR: std::net::Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
const UNDETERMINED_PORT: u16 = 0;

#[derive(Clone, Copy, PartialEq, Debug)]
struct TCPEvent {
    sock_id: SockID,
    kind: TCPEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TCPEventKind {
    ConnectionCompleted,
    Acked,
    DataArrived,
    ConnectionClosed,
}

pub struct TCP {
    sockets: RwLock<HashMap<SockID, Socket>>,
    event_condvar: (Mutex<Option<TCPEvent>>, Condvar),
}

impl TCPEvent {
    fn new(sock_id: SockID, kind: TCPEventKind) -> Self {
        Self { sock_id, kind }
    }
}

impl TCP {
    pub fn new() -> Arc<Self> {
        let sockets = RwLock::new(HashMap::new());
        let tcp = Arc::new(Self {
            sockets,
            event_condvar: (Mutex::new(None), Condvar::new()),
        });

        let cloned_tcp = tcp.clone();
        thread::spawn(move || {
            cloned_tcp.receive_handler().unwrap();
        });

        let cloned_tcp = tcp.clone();
        thread::spawn(move || {
            cloned_tcp.timer();
        });

        tcp
    }

    /// clientのactive openの最初の挙動
    /// ターゲットに接続し, 接続済みソケットのIDを返す
    pub fn connect(&self, addr: Ipv4Addr, port: u16) -> Result<SockID> {
        let mut rng = rand::thread_rng();
        let mut socket = Socket::new(
            get_source_ipv4_addr()?,
            addr,
            self.select_unused_port(&mut rng)?,
            port,
            TcpStatus::SynSent,
        )?;
        socket.send_param.initial_seq = rng.gen_range(1..1 << 31);
        socket.send_tcp_packet(socket.send_param.initial_seq, 0, tcpflags::SYN, &[])?;
        socket.send_param.unacked_seq = socket.send_param.initial_seq;
        socket.send_param.next = socket.send_param.initial_seq + 1;

        let mut sockets = self.sockets.write().unwrap();
        let sock_id = socket.get_sock_id();
        sockets.insert(sock_id, socket);

        // sockets.write()でRwLockから得たwrite lockを外している
        drop(sockets);
        dbg!("wait for the connection completed");
        self.wait_event(sock_id, TCPEventKind::ConnectionCompleted);
        dbg!("connection completed");
        Ok(sock_id)
    }

    /// リスニングソケットを作成し, そのSockIDを返す
    pub fn listen(&self, local_addr: Ipv4Addr, local_port: u16) -> Result<SockID> {
        let socket = Socket::new(
            local_addr,
            UNDETERMINED_IP_ADDR, // サーバ側がlistenを開始した時点では接続先IPアドレスは未定
            local_port,
            UNDETERMINED_PORT, // サーバ側がlistenを開始した時点では接続先portは未定
            TcpStatus::Listen,
        )?;
        let mut sockets = self.sockets.write().unwrap();
        let sock_id = socket.get_sock_id();
        sockets.insert(sock_id, socket);

        // 明示的にdropしなくてもスコープを抜ければやってくれる？
        drop(sockets);

        Ok(sock_id)
    }

    /// 接続済みソケットが生成されるまで待機し, 生成されたらそのIDを返す
    pub fn accept(&self, sock_id: SockID) -> Result<SockID> {
        self.wait_event(sock_id, TCPEventKind::ConnectionCompleted);
        let mut sockets = self.sockets.write().unwrap();

        // キューに詰まったソケットをdeque
        sockets
            .get_mut(&sock_id)
            .context(format!("no such socket: {:?}", sock_id))?
            .connection_queue
            .pop_front()
            .context("no connected socket")
    }

    /// バッファのデータを送信する. 必要であれば複数のパケットに分割して送信する
    /// 全て送信したら(まだackされてなくても)リターンする
    pub fn send(&self, sock_id: SockID, buffer: &[u8]) -> Result<()> {
        let mut cursor = 0;

        while cursor < buffer.len() {
            let mut sockets = self.sockets.write().unwrap();

            let mut socket = sockets
                .get_mut(&sock_id)
                .context(format!("no such socket: {:?}", sock_id))?;

            let mut send_size = cmp::min(
                MSS,
                cmp::min(socket.send_param.window as usize, buffer.len() - cursor),
            );

            // window sizeが枯渇している場合はACKが来てwindow sizeが更新されるまで待機する
            while send_size == 0 {
                dbg!("waiting for the window size updated by ACK");

                // 待機している間にsocketsのロックを持っていると他スレッドがACKを受信できなくなりデッドロックになってしまう
                // そのためここでロックを外しておく必要がある
                drop(sockets);
                self.wait_event(sock_id, TCPEventKind::Acked);

                sockets = self.sockets.write().unwrap();
                socket = sockets
                    .get_mut(&sock_id)
                    .context(format!("no such socket: {:?}", sock_id))?;

                // 新しく更新されたwindow sizeを元にsend_sizeを再計算する
                send_size = cmp::min(
                    MSS,
                    cmp::min(socket.send_param.window as usize, buffer.len() - cursor),
                );
            }

            dbg!("current window size", socket.send_param.window);
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &buffer[cursor..cursor + send_size],
            )?;

            cursor += send_size;
            socket.send_param.next += send_size as u32;
            socket.send_param.window -= send_size as u16;

            // 少しの間ロックを外して待機し, 受信スレッドがACKを受信できるようにしている
            // send_windowが0になるまで送り続け, 送信がブロックされる確率を下げるため
            drop(sockets);
            thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }

    /// データをバッファに読み込んで, 読み込んだサイズを返す. FINを読み込んだ場合は0を返す
    /// パケットが届くまでブロックする
    pub fn recv(&self, sock_id: SockID, buffer: &mut [u8]) -> Result<usize> {
        let mut sockets = self.sockets.write().unwrap();
        let mut socket = sockets
            .get_mut(&sock_id)
            .context(format!("no such socket: {:?}", sock_id))
            .unwrap();

        dbg!(socket.recv_buffer.len());
        dbg!(socket.recv_param.window);
        // 受信サイズはbufferサイズのような気もするが、この出し方はちょっとよく分からない
        let mut received_size = socket.recv_buffer.len() - socket.recv_param.window as usize;

        while received_size == 0 {
            // ペイロードを受信 or FINを受信でスキップ
            match socket.status {
                TcpStatus::CloseWait | TcpStatus::LastAck | TcpStatus::TimeWait => break,
                _ => {}
            }

            // sendと同じようにwait_eventでブロッキングされるため、ここでsocketsのロックを外しておかないとデッドロックに陥る
            drop(sockets);
            dbg!("waiting for incoming data...");
            self.wait_event(sock_id, TCPEventKind::DataArrived);

            sockets = self.sockets.write().unwrap();
            socket = sockets
                .get_mut(&sock_id)
                .context(format!("no such socket: {:?}", sock_id))
                .unwrap();
            received_size = socket.recv_buffer.len() - socket.recv_param.window as usize;
        }
        let copy_size = cmp::min(buffer.len(), received_size);
        buffer[..copy_size].copy_from_slice(&socket.recv_buffer[..copy_size]);
        socket.recv_buffer.copy_within(copy_size.., 0);
        socket.recv_param.window += copy_size as u16;

        Ok(copy_size)
    }

    pub fn close(&self, sock_id: SockID) -> Result<()> {
        let mut sockets = self.sockets.write().unwrap();
        let mut socket = sockets
            .get_mut(&sock_id)
            .context(format!("no such socket: {:?}", sock_id))
            .unwrap();

        socket.send_tcp_packet(
            socket.send_param.next,
            socket.recv_param.next,
            tcpflags::FIN | tcpflags::ACK,
            &[],
        )?;

        socket.send_param.next += 1;
        match socket.status {
            TcpStatus::Established | TcpStatus::CloseWait => {
                if socket.status == TcpStatus::Established {
                    socket.status = TcpStatus::FinWait1;
                } else if socket.status == TcpStatus::CloseWait {
                    socket.status = TcpStatus::LastAck;
                }
                drop(sockets);
                self.wait_event(sock_id, TCPEventKind::ConnectionClosed);
                let mut sockets = self.sockets.write().unwrap();
                sockets.remove(&sock_id);
                dbg!("closed & removed", sock_id);
            }
            TcpStatus::Listen => {
                sockets.remove(&sock_id);
            }
            _ => return Ok(()),
        }

        Ok(())
    }

    fn receive_handler(&self) -> Result<()> {
        dbg!("begin recv thread");
        let (_, mut receiver) = transport::transport_channel(
            655535,
            // IPアドレスが必要なのでLayer3(Ipパケットレベルで取得する)
            TransportChannelType::Layer3(IpNextHeaderProtocols::Tcp),
        )
        .unwrap();

        let mut packet_iter = transport::ipv4_packet_iter(&mut receiver);
        loop {
            // packetは相手視点になるため, こちら視点のlocal_addrは相手視点のremote_addrで, こちら視点のremote_addrは相手視点のlocal_addrとなる
            let (packet, remote_addr) = match packet_iter.next() {
                Ok((p, r)) => (p, r),
                Err(_) => continue,
            };

            let local_addr = packet.get_destination();

            // pnetのTcpPacket作成
            let tcp_packet = match TcpPacket::new(packet.payload()) {
                Some(p) => p,
                None => continue,
            };

            // pnetのTcpPacketから自前定義のTCPPacketを作成
            let packet = TCPPacket::from(tcp_packet);

            let remote_addr = match remote_addr {
                IpAddr::V4(addr) => addr,
                _ => continue,
            };

            let mut sockets = self.sockets.write().unwrap();
            let socket = match sockets.get_mut(&SockID {
                local_addr,
                remote_addr,
                local_port: packet.get_dest(),
                remote_port: packet.get_src(),
            }) {
                // 指定のremote_addr, remote_portでソケットが存在しない場合は新しいコネクションが考えられるため, リスニングソケットを使う
                Some(socket) => socket,
                None => match sockets.get_mut(&SockID {
                    local_addr,
                    remote_addr: UNDETERMINED_IP_ADDR,
                    local_port: packet.get_dest(),
                    remote_port: UNDETERMINED_PORT,
                }) {
                    Some(socket) => socket, // リスニングソケット
                    None => continue,       // どのソケットにも該当しないので無視する
                },
            };

            dbg!("socket.sock_id: ", socket.sock_id);

            if !packet.is_correct_checksum(local_addr, remote_addr) {
                dbg!("invalid checksome");
                continue;
            }

            let sock_id = socket.get_sock_id();
            if let Err(error) = match socket.status {
                TcpStatus::Listen => self.listen_handler(sockets, sock_id, &packet, remote_addr),
                TcpStatus::SynRcvd => self.synrcvd_handler(sockets, sock_id, &packet),
                TcpStatus::SynSent => self.synsent_handler(socket, &packet),
                TcpStatus::Established => self.established_handler(socket, &packet),
                TcpStatus::CloseWait | TcpStatus::LastAck => self.close_handler(socket, &packet),
                TcpStatus::FinWait1 | TcpStatus::FinWait2 => self.finwait_handler(socket, &packet),
                _ => {
                    dbg!("not implemented state");
                    dbg!(packet.get_seq());
                    dbg!(packet.get_ack());
                    dbg!(packet.get_flag());
                    dbg!(socket.send_param);
                    dbg!(socket.recv_param);
                    Ok(())
                }
            } {
                dbg!(error);
            }
        }
    }

    // listen状態のsocketに対してリクエスト(3 way handshakeのSYN要求)が来た際に呼ばれるhandler
    fn listen_handler(
        &self,
        mut sockets: RwLockWriteGuard<HashMap<SockID, Socket>>,
        listening_socket_id: SockID,
        packet: &TCPPacket,
        remote_addr: Ipv4Addr,
    ) -> Result<()> {
        dbg!("listen handler");

        if packet.get_flag() & tcpflags::ACK > 0 {
            // 本来ならRSTをsendする
            return Ok(());
        }

        let listening_socket = sockets
            .get_mut(&listening_socket_id)
            .context(format!("socket_id not found: {:?}", listening_socket_id))?;

        if packet.get_flag() & tcpflags::SYN == 0 {
            return Ok(());
        }

        // SynRcvdのソケットを作ってSYN/ACKを返す
        let mut connection_socket = Socket::new(
            listening_socket.sock_id.local_addr,
            remote_addr,
            listening_socket.sock_id.local_port,
            packet.get_src(),
            TcpStatus::SynRcvd,
        )?;

        connection_socket.recv_param.next = packet.get_seq() + 1;
        connection_socket.recv_param.initial_seq = packet.get_seq();

        connection_socket.send_param.initial_seq = rand::thread_rng().gen_range(1..1 << 31);
        connection_socket.send_param.window = packet.get_window_size();
        connection_socket.send_tcp_packet(
            connection_socket.send_param.initial_seq,
            connection_socket.recv_param.next,
            tcpflags::SYN | tcpflags::ACK,
            &[],
        )?;

        connection_socket.send_param.next = connection_socket.send_param.initial_seq + 1;
        connection_socket.send_param.unacked_seq = connection_socket.send_param.initial_seq;

        // このコネクション自体を生成したリスニングソケットを登録
        connection_socket.listening_socket = Some(listening_socket.get_sock_id());
        dbg!("status: listen -> ", &connection_socket.status);
        sockets.insert(connection_socket.get_sock_id(), connection_socket);

        Ok(())
    }

    // listen_handlerで作ったsynrcvd状態のsocketに対応したhandler
    // 3 way handshakeの最後にclientからACKが来た際に呼ばれる
    // synrcvd状態のsocketをEstablishedにしてリスニングソケットが持つsocket_idのキューに入れる
    fn synrcvd_handler(
        &self,
        mut sockets: RwLockWriteGuard<HashMap<SockID, Socket>>,
        sock_id: SockID,
        packet: &TCPPacket,
    ) -> Result<()> {
        dbg!("synrcvd handler");
        dbg!(packet);
        let socket = sockets.get_mut(&sock_id).unwrap();

        dbg!(packet.get_flag());
        dbg!(socket.send_param.unacked_seq);
        dbg!(packet.get_ack());
        dbg!(socket.send_param.next);

        if packet.get_flag() & tcpflags::ACK > 0
            && socket.send_param.unacked_seq <= packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            socket.recv_param.next = packet.get_seq();
            socket.send_param.unacked_seq = packet.get_ack();
            socket.status = TcpStatus::Established;
            dbg!("status: synrcv -> {}", &socket.status);

            if let Some(listening_socket_id) = socket.listening_socket {
                let listening_socket = sockets.get_mut(&listening_socket_id).unwrap();
                listening_socket.connection_queue.push_back(sock_id);
                self.publish_event(
                    listening_socket.get_sock_id(),
                    TCPEventKind::ConnectionCompleted,
                );
            }
        } else {
            dbg!("synrcv handler failed");
        }

        Ok(())
    }

    // あまり実装がよくない気がする
    fn delete_acked_segment_from_retransmissio_queue(&self, socket: &mut Socket) {
        dbg!(socket.send_param.unacked_seq);

        while let Some(item) = socket.retransmission_queue.pop_front() {
            dbg!(socket.send_param.unacked_seq);
            dbg!(item.packet.get_seq());
            if socket.send_param.unacked_seq > item.packet.get_seq() {
                dbg!("successfully acked");
                socket.send_param.window += item.packet.payload().len() as u16;
                self.publish_event(socket.get_sock_id(), TCPEventKind::Acked);
            } else {
                socket.retransmission_queue.push_front(item);
                break;
            }
        }
    }

    fn established_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("established handler");

        if socket.send_param.unacked_seq < packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            dbg!("pop retransmission queue");
            socket.send_param.unacked_seq = packet.get_ack();
            self.delete_acked_segment_from_retransmissio_queue(socket);
        } else if socket.send_param.next < packet.get_ack() {
            // 未送信セグメントに対するackは破棄
            return Ok(());
        }

        if packet.get_flag() & tcpflags::ACK == 0 {
            // ACKが立ってないパケットは破棄
            return Ok(());
        }

        if !packet.payload().is_empty() {
            self.process_payload(socket, packet)?;
        }

        // クライアント側はパッシブクローズになるため、急にサーバからFINを受け取ることがある(というかいつか必ず終わりが来る)
        if packet.get_flag() & tcpflags::FIN > 0 {
            socket.recv_param.next = packet.get_seq() + 1;
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
            socket.status = TcpStatus::CloseWait;
            self.publish_event(socket.get_sock_id(), TCPEventKind::DataArrived);
        }

        Ok(())
    }

    // SYNSENT状態のソケットに到着したパケットの処理
    fn synsent_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("synsent handler");
        if packet.get_flag() & tcpflags::ACK > 0
            && packet.get_flag() & tcpflags::SYN > 0
            && socket.send_param.unacked_seq <= packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            // synsentの状態で受けるackなので恐らくpacket.get_sequence() + 1 == packet.get_ack()になると考えられる
            // 確認したところならなかった。なぜ？
            socket.recv_param.next = packet.get_seq() + 1;

            // これがよく分からない、nextがわかっている以上なぜこの状態を持っていないといけないのか？
            socket.recv_param.initial_seq = packet.get_seq();

            // これはOK
            socket.send_param.unacked_seq = packet.get_ack();
            socket.send_param.window = packet.get_window_size();

            if socket.send_param.unacked_seq > socket.send_param.initial_seq {
                dbg!("first half");
                socket.status = TcpStatus::Established;

                // ここでactive openしたclientがSYN/ACKに対してSEQ=1, ACK=1のACKを返す
                // ちなみにSEQは相手が欲しいペイロード、ACKはこちらが欲しいペイロードの先頭を指す
                socket.send_tcp_packet(
                    socket.send_param.next,
                    socket.recv_param.next,
                    tcpflags::ACK,
                    &[],
                )?;

                dbg!("status: synsent ->", &socket.status);
                self.publish_event(socket.get_sock_id(), TCPEventKind::ConnectionCompleted);
            } else {
                dbg!("second half");
                // どのシチュエーションの処理かちょっと分からない
                socket.status = TcpStatus::SynRcvd;
                socket.send_tcp_packet(
                    socket.send_param.next,
                    socket.recv_param.next,
                    tcpflags::ACK,
                    &[],
                )?;
                dbg!("status: synsent ->", &socket.status);
            }
        }

        Ok(())
    }

    // FINWAIT1 or FINWAIT2状態のソケットに到着したパケットの処理
    // アクティブクローズ(サーバ側)
    fn finwait_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("finwait handler");
        if socket.send_param.unacked_seq < packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            socket.send_param.unacked_seq = packet.get_ack();
            self.delete_acked_segment_from_retransmissio_queue(socket);
        } else if socket.send_param.next < packet.get_ack() {
            // 未送信セグメントに対するackは破棄
            return Ok(());
        }

        if packet.get_flag() & tcpflags::ACK == 0 {
            // ACKが立ってないパケットは破棄
            return Ok(());
        }

        if !packet.payload().is_empty() {
            self.process_payload(socket, packet)?;
        }

        if socket.status == TcpStatus::FinWait1
            && socket.send_param.next == socket.send_param.unacked_seq
        {
            // 送信したFINがackされていなければFinWait2へ遷移
            socket.status = TcpStatus::FinWait2;
            dbg!("status: finwait1 ->", &socket.status);
        }

        if packet.get_flag() & tcpflags::FIN > 0 {
            // 本来はCLOSING stateも考慮する必要があるが複雑になるので省略する
            socket.recv_param.next += 1;
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
            self.publish_event(socket.get_sock_id(), TCPEventKind::ConnectionClosed);
        }

        Ok(())
    }

    fn close_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("closewiat | lastack handler");
        socket.send_param.unacked_seq = packet.get_ack();
        Ok(())
    }

    fn select_unused_port(&self, rng: &mut ThreadRng) -> Result<u16> {
        for _ in 0..(PORT_RANGE.end - PORT_RANGE.start) {
            let local_port = rng.gen_range(PORT_RANGE);

            let sockets = self.sockets.read().unwrap();
            if sockets
                .keys()
                .all(|sock_id| local_port != sock_id.local_port)
            {
                return Ok(local_port);
            }
        }

        anyhow::bail!("no available port found");
    }

    fn wait_event(&self, sock_id: SockID, kind: TCPEventKind) {
        let (lock, cvar) = &self.event_condvar;
        let mut event = lock.lock().unwrap();

        // cvar.waitで次のイベントの変更通知(notify_all)を待ち、通知がきたらまた次に進む
        // 対象となるsocketが目的の状態(TCPEventKind)になったらeventをNoneにして終了する
        loop {
            dbg!("wait event...");
            if let Some(ref tcp_event) = *event {
                dbg!("match the event sock waited for! break!");
                if tcp_event.sock_id == sock_id && tcp_event.kind == kind {
                    break;
                }
            }

            // cvarがnotifyされるまでeventのロックを外して待機
            dbg!("cvar wait...");
            event = cvar.wait(event).unwrap();
        }

        dbg!(&event);
        *event = None;
    }

    /// 指定のソケットIDにイベントを発行する
    fn publish_event(&self, sock_id: SockID, kind: TCPEventKind) {
        let (lock, cvar) = &self.event_condvar;
        let mut e = lock.lock().unwrap();
        *e = Some(TCPEvent::new(sock_id, kind));
        cvar.notify_all();
    }

    /// タイマースレッド用の関数
    /// 全てのソケットの再送キューを見て、タイムアウトしているパケットを再送する
    fn timer(&self) {
        dbg!("begin timer thread");

        loop {
            let mut sockets = self.sockets.write().unwrap();
            for (sock_id, socket) in sockets.iter_mut() {
                // queueからpopしながら中でpush_backもしてiterateしているためあまりいい実装ではなさそう
                // もう少し良い実装を検討してもいいかもしれない
                while let Some(mut item) = socket.retransmission_queue.pop_front() {
                    // 再送キューからackされたセグメントを除去する
                    // established state以外の時に送信されたセグメントを除去するために必要
                    if socket.send_param.unacked_seq > item.packet.get_seq() {
                        dbg!("successfully acked", item.packet.get_seq());
                        socket.send_param.window += item.packet.payload().len() as u16;
                        self.publish_event(*sock_id, TCPEventKind::Acked);

                        if item.packet.get_flag() & tcpflags::FIN > 0
                            && socket.status == TcpStatus::LastAck
                        {
                            self.publish_event(*sock_id, TCPEventKind::ConnectionClosed);
                        }
                        continue;
                    }

                    // タイムアウトを確認
                    if item.latest_transmission_time.elapsed().unwrap()
                        < Duration::from_secs(RETRANSMITTION_TIMEOUT)
                    {
                        // 取り出したエントリがタイムアウトしてないなら、以降のキューのエントリもタイムアウトしてない
                        // 先頭に戻す
                        socket.retransmission_queue.push_front(item);
                        break;
                    }

                    // ackされてなければ再送
                    if item.transmission_count < MAX_TRANSMITTION {
                        // 再送
                        dbg!("retransmit");

                        socket
                            .sender
                            .send_to(item.packet.clone(), IpAddr::V4(socket.sock_id.remote_addr))
                            .context("failed to retransmit")
                            .unwrap();

                        item.transmission_count += 1;
                        item.latest_transmission_time = SystemTime::now();
                        socket.retransmission_queue.push_back(item);
                        break;
                    } else {
                        dbg!("reached MAX_TRANSMISSION");

                        if item.packet.get_flag() & tcpflags::FIN > 0
                            && (socket.status == TcpStatus::LastAck
                                || socket.status == TcpStatus::FinWait1
                                || socket.status == TcpStatus::FinWait2)
                        {
                            self.publish_event(*sock_id, TCPEventKind::ConnectionClosed);
                        }
                    }
                }
            }
            // ロックを外して待機
            drop(sockets);
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// パケットのペイロードを受信バッファにコピーする
    fn process_payload(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        // バッファにおける読み込みの先頭位置
        dbg!(socket.recv_param.next);
        dbg!(packet.get_seq());

        let offset = socket.recv_buffer.len() - socket.recv_param.window as usize
            + (packet.get_seq() - socket.recv_param.next) as usize;

        let copy_size = cmp::min(packet.payload().len(), socket.recv_buffer.len() - offset);
        socket.recv_buffer[offset..offset + copy_size]
            .copy_from_slice(&packet.payload()[..copy_size]);

        // ロス再送の際に穴埋めされるためにmaxを取る
        socket.recv_param.tail =
            cmp::max(socket.recv_param.tail, packet.get_seq() + copy_size as u32);

        dbg!(offset);
        if packet.get_seq() == socket.recv_param.next {
            // packetの順番が入れ替わってない場合のみrecv_param.nextを進められる
            socket.recv_param.next = socket.recv_param.tail;
            socket.recv_param.window -= (socket.recv_param.tail - packet.get_seq()) as u16;
        }

        if copy_size > 0 {
            // 受信バッファにコピーが成功(受信バッファにまだ余裕がある場合とも言える)
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
        } else {
            // 受信バッファが溢れた時はセグメントを破棄する
            dbg!("recv buffer overflow");
        }
        self.publish_event(socket.get_sock_id(), TCPEventKind::DataArrived);
        Ok(())
    }
}

/*
本家は送信先IPを引数にしてip route getコマンドから送信元IPを取得していたが、以下2つの理由により変更した
少し強めの表現ではあるが、ここのコードに対してであり、TCPのRustによる実装を教えてくれている筆者には感謝している
1. コマンドの実行結果から期待したデータを得るのはダサい(他プロセスを起動させることになってリソース的にも無駄がかなり多い)
2. そもそも送信元IPを取得するのに送信先IPが必要になるのは意味が分からないというか不要

変更するにあたってlocal_ip_addressを採用してみた
https://docs.rs/local-ip-address/latest/local_ip_address/
*/
pub fn get_source_ipv4_addr() -> Result<Ipv4Addr> {
    let addr = local_ip_address::local_ip().unwrap();
    println!("local_addr: {}", addr);
    match addr {
        IpAddr::V4(ipv4_addr) => Ok(ipv4_addr),
        _ => bail!("failed to get ipv4 addr"),
    }
}
