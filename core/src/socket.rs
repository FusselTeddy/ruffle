use crate::{
    avm1::{
        globals::xml_socket::XmlSocket, Activation as Avm1Activation, ActivationIdentifier,
        ExecutionReason, Object as Avm1Object, TObject as Avm1TObject,
    },
    avm2::{
        object::SocketObject, Activation as Avm2Activation, Avm2, EventObject,
        TObject as Avm2TObject,
    },
    backend::navigator::NavigatorBackend,
    context::UpdateContext,
    string::AvmString,
};
use async_channel::{unbounded, Sender as AsyncSender};
use gc_arena::Collect;
use generational_arena::{Arena, Index};
use std::{
    cell::RefCell,
    sync::mpsc::{channel, Receiver, Sender},
    time::Duration,
};

pub type SocketHandle = Index;

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
enum SocketKind<'gc> {
    Avm2(SocketObject<'gc>),
    Avm1(Avm1Object<'gc>),
}

#[derive(Collect)]
#[collect(no_drop)]
struct Socket<'gc> {
    target: SocketKind<'gc>,
    sender: RefCell<AsyncSender<Vec<u8>>>,
}

impl<'gc> Socket<'gc> {
    fn new(target: SocketKind<'gc>, sender: AsyncSender<Vec<u8>>) -> Self {
        Self {
            target,
            sender: RefCell::new(sender),
        }
    }
}

#[derive(Debug)]
pub enum ConnectionState {
    Connected,
    Failed,
    TimedOut,
}

#[derive(Debug)]
pub enum SocketAction {
    Connect(SocketHandle, ConnectionState),
    Data(SocketHandle, Vec<u8>),
    Close(SocketHandle),
}

/// Manages the collection of Sockets.
pub struct Sockets<'gc> {
    sockets: Arena<Socket<'gc>>,

    receiver: Receiver<SocketAction>,
    sender: Sender<SocketAction>,
}

unsafe impl<'gc> Collect for Sockets<'gc> {
    fn trace(&self, cc: &gc_arena::Collection) {
        for (_, socket) in self.sockets.iter() {
            socket.trace(cc)
        }
    }
}

impl<'gc> Sockets<'gc> {
    pub fn empty() -> Self {
        let (sender, receiver) = channel();

        Self {
            sockets: Arena::new(),
            receiver,
            sender,
        }
    }

    pub fn connect_avm2(
        &mut self,
        backend: &mut dyn NavigatorBackend,
        target: SocketObject<'gc>,
        host: String,
        port: u16,
    ) {
        let (sender, receiver) = unbounded();

        let socket = Socket::new(SocketKind::Avm2(target), sender);
        let handle = self.sockets.insert(socket);

        // NOTE: This call will send SocketAction::Connect to sender with connection status.
        backend.connect_socket(
            host,
            port,
            Duration::from_millis(target.timeout().into()),
            handle,
            receiver,
            self.sender.clone(),
        );

        if let Some(existing_handle) = target.set_handle(handle) {
            // As written in the AS3 docs, we are supposed to close the existing connection,
            // when a new one is created.
            self.close(existing_handle)
        }
    }

    pub fn connect_avm1(
        &mut self,
        backend: &mut dyn NavigatorBackend,
        target: Avm1Object<'gc>,
        host: String,
        port: u16,
    ) {
        let (sender, receiver) = unbounded();

        let xml_socket = match XmlSocket::cast(target.into()) {
            Some(xml_socket) => xml_socket,
            None => return,
        };

        let socket = Socket::new(SocketKind::Avm1(target), sender);
        let handle = self.sockets.insert(socket);

        // NOTE: This call will send SocketAction::Connect to sender with connection status.
        backend.connect_socket(
            host,
            port,
            Duration::from_millis(xml_socket.timeout().into()),
            handle,
            receiver,
            self.sender.clone(),
        );

        if let Some(existing_handle) = xml_socket.set_handle(handle) {
            // NOTE: AS2 docs don't specify what happens when connect is called with open connection,
            //       but we will close the existing connection anyway.
            self.close(existing_handle)
        }
    }

    pub fn is_connected(&self, handle: SocketHandle) -> bool {
        matches!(self.sockets.get(handle), Some(Socket { .. }))
    }

    pub fn send(&mut self, handle: SocketHandle, data: Vec<u8>) {
        if let Some(Socket { sender, .. }) = self.sockets.get_mut(handle) {
            let _ = sender.borrow().send_blocking(data);
        }
    }

    pub fn close(&mut self, handle: SocketHandle) {
        if let Some(Socket { sender, target }) = self.sockets.remove(handle) {
            drop(sender); // NOTE: By dropping the sender, the reading task will close automatically.

            // Clear the buffers if the connection was closed.
            match target {
                SocketKind::Avm1(target) => {
                    let target =
                        XmlSocket::cast(target.into()).expect("target should be XmlSocket");

                    target.read_buffer().clear();
                }
                SocketKind::Avm2(target) => {
                    target.read_buffer().clear();
                    target.write_buffer().clear();
                }
            }
        }
    }

    pub fn update_sockets(context: &mut UpdateContext<'_, 'gc>) {
        let mut actions = vec![];

        while let Ok(action) = context.sockets.receiver.try_recv() {
            actions.push(action)
        }

        for action in actions {
            match action {
                SocketAction::Connect(handle, ConnectionState::Connected) => {
                    let target = match context.sockets.sockets.get(handle) {
                        Some(socket) => socket.target,
                        // Socket must have been closed before we could send event.
                        None => continue,
                    };

                    match target {
                        SocketKind::Avm2(target) => {
                            let mut activation = Avm2Activation::from_nothing(context.reborrow());

                            let connect_evt =
                                EventObject::bare_default_event(&mut activation.context, "connect");
                            Avm2::dispatch_event(
                                &mut activation.context,
                                connect_evt,
                                target.into(),
                            );
                        }
                        SocketKind::Avm1(target) => {
                            let mut activation = Avm1Activation::from_stub(
                                context.reborrow(),
                                ActivationIdentifier::root("[XMLSocket]"),
                            );

                            let _ = target.call_method(
                                "onConnect".into(),
                                &[true.into()],
                                &mut activation,
                                ExecutionReason::Special,
                            );
                        }
                    }
                }
                SocketAction::Connect(
                    handle,
                    ConnectionState::Failed | ConnectionState::TimedOut,
                ) => {
                    let target = match context.sockets.sockets.get(handle) {
                        Some(socket) => socket.target,
                        // Socket must have been closed before we could send event.
                        None => continue,
                    };

                    match target {
                        SocketKind::Avm2(target) => {
                            let mut activation = Avm2Activation::from_nothing(context.reborrow());

                            let io_error_evt = activation
                                .avm2()
                                .classes()
                                .ioerrorevent
                                .construct(
                                    &mut activation,
                                    &[
                                        "ioError".into(),
                                        false.into(),
                                        false.into(),
                                        "Error #2031: Socket Error.".into(),
                                        2031.into(),
                                    ],
                                )
                                .expect("IOErrorEvent should be constructed");

                            Avm2::dispatch_event(
                                &mut activation.context,
                                io_error_evt,
                                target.into(),
                            );
                        }
                        // TODO: Not sure if avm1 xmlsocket has a way to notify a error. (Probably should just fire connect event with success as false).
                        SocketKind::Avm1(target) => {
                            let mut activation = Avm1Activation::from_stub(
                                context.reborrow(),
                                ActivationIdentifier::root("[XMLSocket]"),
                            );

                            let _ = target.call_method(
                                "onConnect".into(),
                                &[false.into()],
                                &mut activation,
                                ExecutionReason::Special,
                            );
                        }
                    }
                }
                SocketAction::Data(handle, data) => {
                    let target = match context.sockets.sockets.get(handle) {
                        Some(socket) => socket.target,
                        // Socket must have been closed before we could send event.
                        None => continue,
                    };

                    match target {
                        SocketKind::Avm2(target) => {
                            let mut activation = Avm2Activation::from_nothing(context.reborrow());

                            let bytes_loaded = data.len();
                            target.read_buffer().extend(data);

                            let progress_evt = activation
                                .avm2()
                                .classes()
                                .progressevent
                                .construct(
                                    &mut activation,
                                    &[
                                        "socketData".into(),
                                        false.into(),
                                        false.into(),
                                        bytes_loaded.into(),
                                        //NOTE: bytesTotal is not used by socketData event.
                                        0.into(),
                                    ],
                                )
                                .expect("ProgressEvent should be constructed");

                            Avm2::dispatch_event(
                                &mut activation.context,
                                progress_evt,
                                target.into(),
                            );
                        }
                        SocketKind::Avm1(target) => {
                            let mut activation = Avm1Activation::from_stub(
                                context.reborrow(),
                                ActivationIdentifier::root("[XMLSocket]"),
                            );

                            // NOTE: This is enforced in connect_avm1() function.
                            let xml_socket =
                                XmlSocket::cast(target.into()).expect("target should be XmlSocket");

                            let mut buffer = xml_socket.read_buffer();
                            buffer.extend(data);

                            // Check for a message.
                            while let Some((index, _)) =
                                buffer.iter().enumerate().find(|(_, &b)| b == 0)
                            {
                                let message = buffer.drain(..index).collect::<Vec<_>>();
                                // Remove null byte.
                                let _ = buffer.drain(..1);

                                let message = AvmString::new_utf8_bytes(activation.gc(), &message);

                                let _ = target.call_method(
                                    "onData".into(),
                                    &[message.into()],
                                    &mut activation,
                                    ExecutionReason::Special,
                                );
                            }
                        }
                    }
                }
                SocketAction::Close(handle) => {
                    let target = match context.sockets.sockets.remove(handle) {
                        Some(socket) => socket.target,
                        // Socket must have been closed before we could send event.
                        None => continue,
                    };

                    match target {
                        SocketKind::Avm2(target) => {
                            let mut activation = Avm2Activation::from_nothing(context.reborrow());

                            // Clear the buffers if the connection was closed.
                            target.read_buffer().clear();
                            target.write_buffer().clear();

                            let close_evt =
                                EventObject::bare_default_event(&mut activation.context, "close");
                            Avm2::dispatch_event(&mut activation.context, close_evt, target.into());
                        }
                        SocketKind::Avm1(target) => {
                            let mut activation = Avm1Activation::from_stub(
                                context.reborrow(),
                                ActivationIdentifier::root("[XMLSocket]"),
                            );

                            // Clear the read buffer if the connection was closed.
                            let socket =
                                XmlSocket::cast(target.into()).expect("target should be XmlSocket");

                            socket.read_buffer().clear();

                            let _ = target.call_method(
                                "onClose".into(),
                                &[],
                                &mut activation,
                                ExecutionReason::Special,
                            );
                        }
                    }
                }
            }
        }
    }
}
