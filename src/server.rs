use std::str::{self, FromStr};
use std::net::{SocketAddr, lookup_host, SocketAddrV4};
use std::io::{Read, Write, Result as IoResult, Error as IoError};
use std::os::unix::io::{AsRawFd, RawFd};
use std::mem;
use std::thread::{self, Thread, JoinHandle};
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use spin::{Mutex as SpinLock, RwLock as SpinRwLock};
use std::collections::HashSet;
use mio::tcp::{TcpStream, TcpListener};
use mio::util::Slab;
use mio::{Buf, MutBuf, Token, EventLoop, EventSet, PollOpt, Timeout, Handler, Sender};
use threadpool::ThreadPool;
use num_cpus::get as get_num_cpus;
use rustc_serialize::json;

use queue::*;
use queue_backend::Message;
use config::*;
use protocol::*;
use utils::*;

const SERVER: Token = Token(0);
const FIRST_CLIENT: Token = Token(1);

#[derive(Debug)]
pub enum NotifyMessage {
    Response,
}

#[derive(Debug)]
pub enum TimeoutMessage {
    Awake,
    Maintenance
}

#[derive(Debug)]
struct Connection {
    token: Token,
    stream: TcpStream,
    request: RequestBuffer,
    response: Option<ResponseBuffer>,
    interest: EventSet,
    chann: Sender<(Token, NotifyMessage)>,
    processing: bool,
    hup: bool,
}

#[derive(Debug)]
struct ServerQueue {
    queue: Queue,
    waiting_clients: SpinLock<Vec<Token>>
}

// #[derive(Debug)]
pub struct Server {
    config: ServerConfig,
    queues: SpinRwLock<HashMap<String, Arc<ServerQueue>>>,
    listener: TcpListener,
    thread_pool: ThreadPool,
}

pub struct ServerHandler {
    server: Server,
    connections: Slab<Connection>,
}

type ResponseResult = Result<ResponseBuffer, Status>;

impl Server {
    #[inline]
    fn get_queue(&self, name: &str) -> Option<Arc<ServerQueue>> {
        self.queues.read().get(name).map(|q| q.clone())
    }

    fn delete_queue(&self, name: &str) -> Option<Arc<ServerQueue>> {
        let result = self.queues.write().remove(name);
        if let Some(ref sq) = result {
            sq.queue.as_mut().delete();
        }
        result
    }

    fn list_queues(&self) -> String {   
        let queue_names: Vec<_> = self.queues.read().keys().cloned().collect();
        json::encode(&queue_names).unwrap()
    }

    fn get_or_create_queue(&self, name: &str) -> Arc<ServerQueue> {
        if let Some(sq) = self.get_queue(name) {
            return sq
        }
        let server_config = &self.config;
        self.queues.write().entry(name.into()).or_insert_with(|| {
            info!("Creating queue {:?}", name);
            let inner_queue = Queue::new(server_config.new_queue_config(name), true);
            trace!("done creating queue {:?}", name);

            Arc::new(ServerQueue {
                queue: inner_queue,
                waiting_clients: Default::default(),
            })
        }).clone()
    }
}

fn write_response(stream: &mut TcpStream, response: &mut ResponseBuffer) -> IoResult<usize> {
    let bytes = response.bytes();
    if bytes.is_empty() && response.remaining() != 0 {
        trace!("response.bytes().is_empty() && response.remaining() == {}", response.remaining());
        if let Some((fd, fd_offset)) = response.send_file_opt {
            let r = sendfile(stream.as_raw_fd(), fd, fd_offset, response.remaining());
            trace!("sendfile returned {}", r);
            if r == -1 {
                Err(IoError::from_raw_os_error(r as i32))
            } else {
                Ok(r as usize)
            }
        } else {
            unreachable!();
        }
    } else {
        stream.write(response.bytes())
    }
}

impl Connection {
    fn split_colon(composed: &str) -> (&str, Option<&str>) {
        if let Some(pos) = composed.find(':') {
            (&composed[..pos], Some(&composed[pos + 1..]))
        } else {
            (composed, None)
        }
    }

    fn get(&self, server: &Server, opcode: OpCode, key_str_slice: &str) -> ResponseResult {
        let (queue_name, channel_name_opt) = Self::split_colon(key_str_slice);
        let channel_name = channel_name_opt.unwrap();
        let sq_opt = server.get_queue(queue_name);
        if let Some(sq) = sq_opt {
            if let Some(message) = sq.queue.as_mut().get(channel_name) {
                if let Some(send_file) = message.send_file_opt {
                    let send_file = message.send_file_opt.unwrap();
                    Ok(ResponseBuffer::new_get_response_fd(&self.request, message.id, send_file.0, send_file.1, send_file.2))
                } else {
                    Ok(ResponseBuffer::new_get_response(&self.request, message.id, message.body))
                }
            } else {
                debug!("queue {:?} channel {:?} has no messages", queue_name, channel_name);
                sq.waiting_clients.lock().push(self.token);
                Err(Status::KeyNotFound)
            }
        } else {
            debug!("queue {:?} not found", queue_name);
            Err(Status::InvalidArguments)
        }
    }

    fn put(&self, server: &Server, opcode: OpCode, key_str_slice: &str, value_slice: &[u8]) -> ResponseResult {
        let (queue_name, channel_name_opt) = Self::split_colon(key_str_slice);
        let sq = server.get_or_create_queue(queue_name);

        if let Some(channel_name) = channel_name_opt {
            info!("creating queue {:?} channel {:?}", queue_name, channel_name);
            sq.queue.as_mut().create_channel(channel_name);
        } else {
            debug!("inserting into {:?} {:?}", key_str_slice, value_slice);
            let id = sq.queue.as_mut().put(&Message{id: 0, body: value_slice, send_file_opt: None}).unwrap();
            trace!("inserted message into {:?} with id {:?}", key_str_slice, id);
        }

        Ok(ResponseBuffer::new_set_response())
    }

    fn delete(&self, server: &Server, opcode: OpCode, key_str_slice: &str) -> ResponseResult {
        let (queue_name, _opt) = Self::split_colon(key_str_slice);

        let (command_name, id_str_opt) = Self::split_colon(_opt.unwrap());
        if command_name.starts_with('_') {
            match command_name {
                "_purge" => {
                    if let Some(sq) = server.get_queue(queue_name) {
                        sq.queue.as_mut().purge();
                    } else {
                        return Err(Status::KeyNotFound);
                    }
                },
                "_delete" => {
                    if let None = server.delete_queue(queue_name) {
                        return Err(Status::KeyNotFound);
                    }
                },
                _ => return Err(Status::InvalidArguments)
            }
            return Ok(ResponseBuffer::new(opcode, Status::NoError));
        }

        let sq = if let Some(queue) = server.get_queue(queue_name) {
            queue
        } else {
            return Err(Status::KeyNotFound)
        };

        if let Some(id_str) = id_str_opt {
            let channel_name = command_name;
            let id = if let Ok(id) = id_str_opt.unwrap().parse() {
                id
            } else {
                return Err(Status::InvalidArguments)
            };
            debug!("deleting message {:?} from {:?}", id, command_name);
            if sq.queue.as_mut().ack(channel_name, id).is_none() {
                return Err(Status::KeyNotFound)
            }
        } else {
            debug!("deleting channel {:?}", command_name);
            if ! sq.queue.as_mut().delete_channel(command_name) {
                return Err(Status::KeyNotFound)
            }
        }
        Ok(ResponseBuffer::new(opcode, Status::NoError))
    }

    fn dispatch(&mut self, server: &Server) {
        let opcode = self.request.opcode();

        let key_str_slice = unsafe { str::from_utf8_unchecked(self.request.key_slice()) };
        let value_slice = self.request.value_slice();

        debug!("dispatch {:?} {:?} {:?} {:?}", self.token, opcode, key_str_slice, value_slice);

        let response_result = match opcode {
            OpCode::Get | OpCode::GetK | OpCode::GetQ | OpCode::GetKQ
            if value_slice.is_empty() && !key_str_slice.is_empty() => {
                self.get(server, opcode, key_str_slice)
            }
            OpCode::Set if !key_str_slice.is_empty() => {
                self.put(server, opcode, key_str_slice, value_slice)
            }
            OpCode::Delete if !key_str_slice.is_empty() && value_slice.is_empty() => {
                self.delete(server, opcode, key_str_slice)
            }
            OpCode::NoOp if key_str_slice.is_empty() && value_slice.is_empty() => {
                Ok(ResponseBuffer::new(opcode, Status::NoError))
            }
            _ => Err(Status::InvalidArguments)
        };

        self.response = Some(match response_result {
            Ok(response) => response,
            Err(status) => ResponseBuffer::new(opcode, status)
        });

        self.chann.send((self.token, NotifyMessage::Response)).unwrap();
    }

    fn dispatch_in_pool(&mut self, server: &Server) {
        debug!("dispatching request {:?} for token {:?}", self.request, self.token);
        assert!(!self.processing);
        assert!(self.request.is_complete());
        self.processing = true;
        let connection_ptr: &'static mut Self = unsafe { mem::transmute(self as *mut _) };
        let server_ptr: &'static mut Server = unsafe { mem::transmute(server as *const _) };
        server.thread_pool.execute(move || {
            debug_assert!(connection_ptr.processing);
            connection_ptr.dispatch(server_ptr);
        });
    }

    fn new(token: Token, stream: TcpStream, chann: Sender<(Token, NotifyMessage)>) -> Connection {
        Connection {
            stream: stream,
            token: token,
            request: RequestBuffer::new(),
            response: None,
            interest: EventSet::all() - EventSet::writable(),
            chann: chann,
            processing: false,
            hup: false,
        }
    }

    fn ready(&mut self, server: &mut Server, event_loop: &mut EventLoop<ServerHandler>, events: EventSet) -> bool {
        if events.is_hup() || events.is_error() {
            debug!("received events {:?} for token {:?}", events,  self.token);
            event_loop.deregister(&self.stream).unwrap();
            self.hup = true;
            return self.processing
        }

        if events.is_readable() {
            while let Ok(bytes_read) = self.stream.read(self.request.mut_bytes()) {
                if bytes_read == 0 {
                    break
                }
                self.request.advance(bytes_read);
                trace!("filled request with {} bytes, remaining: {}", bytes_read, self.request.remaining());
                
                if self.request.is_complete() {
                    self.interest = EventSet::all() - EventSet::readable() - EventSet::writable();
                    event_loop.reregister(&self.stream, self.token, self.interest, PollOpt::level()).unwrap();
                    self.dispatch_in_pool(server);
                    break
                } else {
                    // keep reading
                }
            }
        }

        if events.is_writable() {
            let response = self.response.as_mut().expect("writable with None response");
            while let Ok(bytes_written) = write_response(&mut self.stream, response) {
                response.advance(bytes_written);
                trace!("filled response with {} bytes, remaining: {}", bytes_written, response.remaining());

                if response.is_complete() {
                    debug!("done sending response {:?} to token {:?}", response, self.token);
                    self.interest = EventSet::all() - EventSet::writable();
                    event_loop.reregister(&self.stream, self.token, self.interest, PollOpt::level()).unwrap();
                    if response.opcode() == OpCode::Exit {
                        return false;
                    }
                    break
                } else {
                    // keep writing
                }
            }
        }
        true
    }

    fn notify(&mut self, server: &mut Server, event_loop: &mut EventLoop<ServerHandler>, msg: NotifyMessage) -> bool {
        if self.hup {
            return false;
        }

        match msg {
            NotifyMessage::Response => {
                assert!(self.processing);
                assert!(self.response.is_some());
                self.request.clear();
                self.processing = false;
                self.interest = EventSet::all() - EventSet::readable();
                event_loop.reregister(&self.stream, self.token, self.interest, PollOpt::level()).unwrap();
            }
            //_ => panic!("can't handle msg {:?}", msg)
        }
        true
    }
}

impl Server {
    fn maintenance_thread_fn(&self) {
        // add code here
    }

    fn awaken_monitor_thread_fn(&self) {
        // add code here
    }

    pub fn new(config: ServerConfig) -> (ServerHandler, EventLoop<ServerHandler>) {
        let addr = SocketAddr::from_str(&config.bind_address).unwrap();

        debug!("binding tcp socket to {:?}", addr);
        let listener = TcpListener::bind(&addr).unwrap();

        let num_cpus = get_num_cpus();
        let num_threads = num_cpus + 2;
        debug!("detected {} cpus, using {} threads", num_cpus, num_threads);

        let server = Server {
            listener: listener,
            config: config,
            queues: Default::default(),
            thread_pool: ThreadPool::new(num_threads)
        };

        let mut event_loop = EventLoop::new().unwrap();
        event_loop.register_opt(&server.listener, SERVER, EventSet::all() - EventSet::writable(), PollOpt::level()).unwrap();

        let server_handler = ServerHandler {
            server: server,
            connections: Slab::new_starting_at(FIRST_CLIENT, 1024)
        };

        (server_handler, event_loop)
    }

     fn ready(&mut self, connections: &mut Slab<Connection>, event_loop: &mut EventLoop<ServerHandler>, events: EventSet) -> bool {
        assert_eq!(events, EventSet::readable());

        if let Some(stream) = self.listener.accept().unwrap() {
            let connection_addr = stream.peer_addr();
            debug!("incomming connection from {:?}", connection_addr);

            // Don't buffer output in TCP - kills latency sensitive benchmarks
            // TODO: use TCP_CORK
            stream.set_nodelay(true).unwrap();

            let token = connections.insert_with(
                |token| Connection::new(token, stream, event_loop.channel())).unwrap();

            debug!("assigned token {:?} to client {:?}", token, connection_addr);

            event_loop.register_opt(
                &connections[token].stream,
                token,
                connections[token].interest,
                PollOpt::level()
            ).unwrap();
        }
        true
    }

    fn notify(&mut self, connections: &mut Slab<Connection>, event_loop: &mut EventLoop<ServerHandler>, msg: NotifyMessage) -> bool {
        panic!("can't handle msg {:?}", msg);
        // true
    }
}

impl Handler for ServerHandler {
    type Timeout = TimeoutMessage;
    type Message = (Token, NotifyMessage);

    #[inline]
    fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token, events: EventSet) {
        trace!("events {:?} for token {:?}", events, token);
        let is_ok = match token {
            SERVER => self.server.ready(&mut self.connections, event_loop, events),
            token => if let Some(connection) = self.connections.get_mut(token) {
                connection.ready(&mut self.server, event_loop, events)
            } else {
                trace!("token {:?} not found", token);
                false
            }
        };
        if !is_ok {
            trace!("deregistering token {:?}", token);
            self.connections.remove(token);
        }

        trace!("done events {:?} for token {:?}", events, token);
    }

    #[inline]
    fn notify(&mut self, event_loop: &mut EventLoop<Self>, composed_msg: Self::Message) {
        let (token, message) = composed_msg;
        trace!("notify event for token {:?} with {:?}", token, message);
        let is_ok = match token {
            SERVER => self.server.notify(&mut self.connections, event_loop, message),
            token => if let Some(connection) = self.connections.get_mut(token) {
                connection.notify(&mut self.server, event_loop, message)
            } else {
                trace!("token {:?} not found", token);
                false
            }
        };
        if !is_ok {
            trace!("deregistering token {:?}", token);
            self.connections.remove(token);
        }
        trace!("end notify event for token {:?}", token);
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
        match timeout {
            TimeoutMessage::Awake => (),
            TimeoutMessage::Maintenance => ()
        }
    }

    fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
        panic!("interrupted");
    }
}

use libc::{c_void, size_t, off_t, ssize_t};
mod ffi {
    use std::os::unix::io::RawFd;
    use libc::{c_void, size_t, off_t, ssize_t};
    extern {
        pub fn sendfile(out_fd: RawFd, in_fd: RawFd, offset: *mut off_t, count: size_t) -> ssize_t;
    }
}

fn sendfile(out_fd: RawFd, in_fd: RawFd, offset: usize, count: usize) -> isize {
    unsafe {
        let mut offset = offset as off_t;
        ffi::sendfile(out_fd, in_fd, &mut offset, count as size_t) as isize
    }
}
