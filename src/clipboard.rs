use std::collections::{hash_map::Entry, HashMap};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use os_pipe::{pipe, PipeReader};

use spdlog::prelude::*;

use tokio::io::unix::AsyncFd;
use tokio::select;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::error::{log_and_pass_on, Anyhow};
use crate::pincer::{Pincer, SeatPincerMap};
use crate::register::MimeType;
use crate::seat::{ClipboardState, SeatData, SeatIdentifier};

/// Enum for messages that can be passed to a [`Clipboard`] instance
#[derive(Clone, Debug)]
pub enum ClipboardMessage {
    OfferOnSeat(String),
    GrabSeat(WlSeat),
}

/// Transmitting end of a channel for passing messages to a [`Clipboard`] instance
pub type ClipboardTx = broadcast::Sender<ClipboardMessage>;

/// Struct that interfaces with the Wayland compositor
#[derive(Debug)]
pub struct Clipboard {
    queue: Arc<Mutex<EventQueue<Clipboard>>>,
    pincers: Arc<Mutex<SeatPincerMap>>,
    manager: ZwlrDataControlManagerV1,
    seats: HashMap<WlSeat, SeatData>,
    offers: HashMap<ZwlrDataControlOfferV1, HashMap<MimeType, PipeReader>>,
    /// Receiving end of a channel that can be used to pass messages to this `Clipboard`
    rx: broadcast::Receiver<ClipboardMessage>,
    /// Transmitting end of a channel that can be used to pass messages to this `Clipboard`
    tx: broadcast::Sender<ClipboardMessage>,
}

impl Clipboard {
    /// Create a new `Clipboard` instance that is connected to the Wayland compositor.
    ///
    /// # Arguments
    ///
    /// * `pincers` - Reference to the pool of Pincers to be shared between this `Clipboard` and a
    ///   [`Daemon`](crate::daemon::Daemon) instance
    pub fn new(pincers: Arc<Mutex<SeatPincerMap>>) -> Result<Self, Anyhow> {
        // Connect to the Wayland compositor
        let conn = Connection::connect_to_env()
            .map_err(log_and_pass_on!("Could not connect to to Wayland"))?;

        // Initialize an event queue for the registry
        let (globals, queue) = registry_queue_init(&conn).map_err(log_and_pass_on!(
            "Could not initialize Wayland registry queue"
        ))?;

        // Verify that the compositor supports the protocol
        let qh = &queue.handle();
        let data_control_version = 1;
        let manager = globals
            // This type annotation is superfluous, but illustrates how to specify the type of
            // global to bind.
            .bind::<ZwlrDataControlManagerV1, _, _>(
                qh,
                data_control_version..=data_control_version,
                (),
            )
            .map_err(log_and_pass_on!(format!(
                "Compositor does not support {} v{data_control_version}",
                ZwlrDataControlManagerV1::interface().name
            )))?;

        // Find out which seats exist
        let registry = globals.registry();
        let seats = globals.contents().with_list(|globals| {
            globals
                .iter()
                .filter(|global| {
                    global.interface == WlSeat::interface().name && global.version >= 2
                })
                .map(|global| {
                    let seat = registry.bind(global.name, 2, qh, ());
                    let sd = SeatData {
                        numeric_name: global.name,
                        ..Default::default()
                    };
                    (seat, sd)
                })
                .collect()
        });

        let queue = Arc::new(Mutex::new(queue));
        let (tx, rx) = broadcast::channel::<ClipboardMessage>(8);
        let mut clip = Clipboard {
            queue,
            pincers,
            manager,
            seats,
            offers: HashMap::new(),
            rx,
            tx,
        };
        clip.grab_all()?;

        Ok(clip)
    }

    /// Start listening to events from the Wayland compositor
    ///
    /// # Arguments
    ///
    /// * `token` - A [`CancellationToken`] that can be used to cancel listening
    pub async fn listen(&mut self, token: CancellationToken) -> Result<(), Anyhow> {
        loop {
            // cloning prevents referencing self
            let q = self.queue.clone();
            let read_guard = {
                let mut q = q.lock().unwrap();
                let _ = q
                    .flush()
                    .map_err(|e| warn!("Wayland communication error: {e}"));
                let _ = q
                    .dispatch_pending(self)
                    .map_err(|e| warn!("Wayland communication error: {e}"));
                q.prepare_read().unwrap()
                // lock is released here
            };
            let poll_wl = async move {
                {
                    let conn = read_guard.connection_fd();
                    let fd = AsyncFd::new(conn.as_fd()).unwrap();
                    let _ = fd.readable().await;
                }
                read_guard.read()
            };
            let poll_channel = async { self.rx.recv().await };
            select! {
                _ = token.cancelled() => break,
                read = poll_wl => {
                    let mut q = q.lock().unwrap();
                    match read {
                        Ok(_) => {
                            let _ = q
                                .dispatch_pending(self)
                                .map_err(|e| warn!("Wayland communication error: {e}"));
                        }
                        Err(e) => {
                            warn!("Wayland communication error: {e}");
                        }
                    };
                },
                Ok(msg) = poll_channel => {
                    use ClipboardMessage::*;
                    match msg {
                        GrabSeat(seat) => {
                            // Safety: this branch cannot be called except if the seat is registered, so it
                            // is safe to call unwrap
                            trace!("Received a message to grab clipboard of {} = {}", seat.id(), self.seats.get(&seat).unwrap());
                            let _ = self.grab(std::iter::once(&seat));
                        }
                        OfferOnSeat(seat) => {
                            trace!("Received a message to offer on seat {seat}");
                            let _ = self.offer(&seat);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn send<T>(&self, seat: &WlSeat, mime: MimeType, fd: T)
    where
        T: Into<File>,
    {
        let Some(seat_data) = self.seats.get(seat) else {
            warn!("Received request from unknown seat {}", seat.id());
            return;
        };
        let Some(ref seat_name) = seat_data.name else {
            warn!("Received request from unknown seat {}", seat.id());
            return;
        };
        let pincers = self.pincers.lock().unwrap();
        let Some(pincer) = pincers.get(seat_name) else {
            warn!(
                "Clipboard of seat {} = {seat_data} is not managed",
                seat.id()
            );
            return;
        };
        let reg = pincer.get_active_address();
        debug!("Received request for {mime}, using {reg}",);
        match pincer.paste(&mime) {
            Some((_, mime, data)) => {
                let mut target_file: File = fd.into();
                match target_file.write(data) {
                    Ok(n) => {
                        debug!("Sent {n} bytes of type {mime} from {reg}",)
                    }
                    Err(e) => warn!("Could not send data: {e}"),
                }
            }
            None => {
                debug!("{mime} was not found in {reg}")
            }
        }
    }

    /// Take ownership of the clipboards of all seats
    fn grab_all(&mut self) -> Result<(), Anyhow> {
        // cloning is necassary to avoid re-borrowing when we need to call a mutable method
        let iter = self.seats.keys().cloned().collect::<Vec<_>>();
        self.grab(&iter)
    }

    /// For each given seat, read its current clipboard state into the corresponding [`Pincer`]
    /// instance, then take ownership of its clipboard.
    fn grab<'a, I>(&mut self, seats: I) -> Result<(), Anyhow>
    where
        // + Clone is necessary because we need to iterate over the seats at least twice in order
        // to minize Wayland roundtrips
        I: IntoIterator<Item = &'a WlSeat> + Clone,
    {
        // Request each seat's data device
        let qh = self.queue.lock().unwrap().handle();
        for seat in seats.clone() {
            debug!("Trying to grab clipboard of {}", seat.id());
            if let Some(data) = self.seats.get_mut(seat) {
                if data.device.is_none() {
                    let device = self.manager.get_data_device(seat, &qh, seat.clone());
                    data.set_device(Some(device));
                }
            }
        }

        // After this round-trip we will know about all MIME types and have sent requests for them
        // ...
        self.roundtrip()?;
        // ... and after this round-trip the compositor will have written the data to the pipes
        self.roundtrip()?;

        // Request all the data!
        let xs: Vec<_> = seats
            .clone()
            .into_iter()
            .filter_map(|s| Some(s).zip(self.seats.get(s)))
            .filter_map(|(s, data)| {
                let offer = if let ClipboardState::Offer(ref offer) = data.clipboard_state {
                    Some(offer.clone())
                } else {
                    trace!("Seat {} = {data} does not have an offer", s.id());
                    None
                };
                Option::zip(data.name.clone(), offer)
            })
            // have to collect rather than use the iterator directly because the iterator will have
            // an immutable borrow of self
            .collect();
        for (name, offer) in xs {
            self.yank_from_selection(&name, &offer)?;
        }

        // Grab the clipboards
        for (seat, data) in &mut self.seats {
            let ClipboardState::Offer(ref offer) = data.clipboard_state else {
                continue;
            };
            let source = self.manager.create_data_source(&qh, seat.clone());
            let offer = offer.clone();
            data.set_clipboard_state(ClipboardState::Source(source.clone()));
            if let Some(mimes) = self.offers.get(&offer) {
                for mime in mimes.keys() {
                    source.offer(mime.to_string());
                }
            }
            if let Some(ref d) = data.device {
                d.set_selection(Some(&source));
            }
        }
        self.roundtrip()?;

        Ok(())
    }

    fn yank_from_selection(
        &mut self,
        seat: &SeatIdentifier,
        offer: &ZwlrDataControlOfferV1,
    ) -> Result<(), Anyhow> {
        let mimes = self.offers.get(offer).ok_or(anyhow::Error::msg(format!(
            "Offer {} not registered",
            offer.id()
        )))?;
        let mut pincers = self.pincers.lock().unwrap();
        let pincer = pincers.get_mut(seat).ok_or_else(|| {
            error!("No pincer");
            anyhow::Error::msg(format!("No pincer found for {seat}"))
        })?;

        let read_data = |(mime, mut f): (&MimeType, &PipeReader)| {
            let mut data = Vec::new();
            match f.read_to_end(&mut data) {
                Ok(n) => {
                    debug!("Read {n} bytes of MIME {mime}");
                    Some((mime.clone(), data))
                }
                Err(e) => {
                    error!("While trying to yank data of MIME {mime}, could not read data from {f:?}: {e}");
                    None
                }
            }
        };
        pincer
            .yank(mimes.iter().filter_map(read_data))
            .map(|_| {})
            .map_err(anyhow::Error::msg)
    }

    fn offer(&mut self, name: &String) -> Result<(), Anyhow> {
        let seat = {
            let Some((seat, _)) = self
                .seats
                .iter()
                .find(|(_, v)| v.name.as_ref() == Some(name))
            else {
                warn!("Received a message to offer data on seat {name}, but it is not registered");
                anyhow::bail!("Unregistered seat");
            };
            seat.clone()
        };

        if let Some(data) = self.seats.get_mut(&seat) {
            data.set_clipboard_state(ClipboardState::Switching)
        };
        // We need to roundtrip here because if we owned the selection previously, setting the
        // state to Switching will destroy our ZwlrDataControlSourceV1 proxy, which will emit a
        // "selection no longer valid" event ( ZwlrDataControlDevice::Selection(None) ). We must
        // ensure that this event is received while still in the Switching state, i.e., before
        // setting it to Source below.
        self.roundtrip()?;

        // This block needs an immutable borrow of self, but the roundtrip below needs a mutable
        // borrow. The same effect could be achieved by cloning the Arc<Mutex<_>> first, as that
        // would release the borrow on self. That's cheap, but this way is somewhat clearer to read.
        {
            let pincers = self.pincers.lock().unwrap();
            let Some(pincer) = pincers.get(name) else {
                warn!("Received a message to offer data on seat {name}, but it is not managed");
                anyhow::bail!("Unmanaged seat");
            };

            let address = pincer.get_active_address();
            let register = pincer.get_active_register();
            debug!("Offering data on {} ({name}) from {address}", seat.id());

            let qh = self.queue.lock().unwrap().handle();
            let source = self.manager.create_data_source(&qh, seat.clone());
            // Safety: seat is guaranteed to be a key in self.seats
            let data = self.seats.get_mut(&seat).unwrap();
            data.set_clipboard_state(ClipboardState::Source(source.clone()));
            for mime in register.keys() {
                // MediaTypeBuf has a .to_string() method, but it's not guaranteed to round-trip.
                // This is a clone of the underlying String, so it round-trips.
                source.offer(mime.as_str().to_string());
            }
            if let Some(ref d) = data.device {
                d.set_selection(Some(&source));
            }
        }
        // Immutable borrows of self are released here

        self.roundtrip()?;

        Ok(())
    }

    fn roundtrip(&mut self) -> Result<(), Anyhow> {
        let q = self.queue.clone();
        let res = q.lock().unwrap().roundtrip(self);
        res.map(|_| {}).map_err(|e| {
            error!("Wayland communication error: {e}");
            anyhow::Error::new(e)
        })
    }

    /// Get a [`Sender`](broadcast::Sender) that can be used to pass messages to this `Clipboard`
    pub fn get_tx(&self) -> broadcast::Sender<ClipboardMessage> {
        self.tx.clone()
    }
}

/// Events are dispatched on the [`WlRegistry`] when global objects appear or disappear
impl Dispatch<WlRegistry, GlobalListContents> for Clipboard {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &wayland_client::Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        use wl_registry::Event::*;
        match event {
            // A new global appeared, it could be a Seat. If so, we should prepare to manage its
            // clipboard.
            Global {
                name,
                interface,
                version,
            } => {
                if interface == WlSeat::interface().name && version >= 2 {
                    info!("Seat #{name} appeared, preparing to manage its clipboard");
                    let seat = registry.bind(name, 2, qhandle, ());
                    let sd = SeatData {
                        numeric_name: name,
                        ..Default::default()
                    };
                    state.seats.insert(seat, sd);
                }
            }
            // A global disappeared, it could be a Seat. If so, we should un-manage it.
            GlobalRemove { name } => {
                if let Some((_, seat_data)) =
                    state.seats.iter().find(|(_, sd)| sd.numeric_name == name)
                {
                    info!("Seat {seat_data:?} disappeared, un-managing its clipboard");
                    let mut pincers = state.pincers.lock().unwrap();
                    if seat_data
                        .name
                        .as_ref()
                        .and_then(|n| pincers.remove(n))
                        .is_none()
                    {
                        warn!("Tried to unmanage clipboard of seat {seat_data:?}, but it was not managed");
                    }
                }
                state.seats.retain(|_, sd| sd.numeric_name != name);
            }
            _ => {}
        }
    }
}

/// Events are dispatched on a [`WlSeat`] to notify us of its (string) name and its capabilities (input
/// devices) We only care about the name. A [`Pincer`] instance is created for each seat, accessed by
/// name.
impl Dispatch<WlSeat, ()> for Clipboard {
    fn event(
        state: &mut Clipboard,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Clipboard>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            let data = state.seats.get_mut(seat).unwrap();
            data.set_name(name.clone());
            debug!("Registered seat {} = {data}", seat.id());
            let mut pincers = state.pincers.lock().unwrap();
            if let Entry::Vacant(e) = pincers.entry(name) {
                e.insert(Pincer::new());
                info!("Managing clipboard for {} = {data}", seat.id());
            } else {
                warn!("Clipboard of seat {data} already managed");
            }
        }
    }
}

/// This impl is necessary to satisfy trait bounds, but
/// [`ZwlrDataControlManager`](ZwlrDataControlManagerV1) has no events, so it can be a no-op.
impl Dispatch<ZwlrDataControlManagerV1, ()> for Clipboard {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

/// The [`ZwlrDataControlDevice`](ZwlrDataControlDeviceV1) emits events when a selection appears or disappears.
/// * The [`DataOffer`](zwlr_data_control_device_v1::Event::DataOffer) event signifies that the Device has data to offer, i.e. that seat has copied
///   something.
/// * The [`Selection`](zwlr_data_control_device_v1::Event::Selection) event is emitted when the seat's selection is set or unset
/// * The [`Finished`](zwlr_data_control_device_v1::Event::Finished) event notifies us that the seat no longer has a valid offer
impl Dispatch<ZwlrDataControlDeviceV1, WlSeat> for Clipboard {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        seat: &WlSeat,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event::*;
        use ClipboardState::*;
        match event {
            // This event is dispatched to notify us of a Device's Offer. We handle it by preparing
            // a new MIME -> buffer map for the offer.
            DataOffer { id } => {
                let Some(seat) = state.seats.get(seat) else {
                    return;
                };
                match seat.clipboard_state {
                    // If we own the clipboard, ignore this offer
                    Source(_) => {}
                    _ => {
                        state.offers.insert(id, HashMap::new());
                    }
                }
            }
            // This event is dispatched after we have received all the MIME types for the offer
            // indicated, or when the selection is unset
            Selection { id } => {
                let Some(data) = state.seats.get_mut(seat) else {
                    return;
                };
                // The protocol demands that the previous offer will be destroyed; this will happen
                // automatically because after these calls we no longer hold any references to our
                // proxies.
                if let Offer(ref offer) = data.clipboard_state {
                    state.offers.remove(offer);
                }
                if let Some(offer) = id {
                    // If we own the clipboard, ignore this to prevent loopback. We will receive a
                    // ZwlrDataControlSource::Cancelled event to notify us that we don't own the
                    // clipboard anymore, which will set the state to Uninitialized, so this is safe.
                    match data.clipboard_state {
                        // If we own the clipboard, ignore this -- this was us setting the
                        // selection
                        Source(_) | Switching => {}
                        _ => {
                            debug!("Selection set by another client on {} = {data}", seat.id());
                            data.set_clipboard_state(Offer(offer));
                            // We cannot call grab() directly here. grab() needs to issue Wayland roundtrips,
                            // which creates a deadlock because roundtrip() cannot return before this function.
                            // We need to pass a message so that grab() will be called after this function
                            // returns.
                            trace!("Passing a message to grab {} = {data}", seat.id());
                            let _ = state.tx.send(ClipboardMessage::GrabSeat(seat.clone()));
                        }
                    }
                } else {
                    debug!(
                        "Selection on {} = {data} no longer valid {:?}",
                        seat.id(),
                        data.clipboard_state
                    );
                    match &data.clipboard_state {
                        Switching => {
                            debug!("In the process of switching");
                        }
                        _ => {
                            data.set_clipboard_state(Unavailable);
                        }
                    }
                }
            }
            // This event is dispatched to notify us that this device is no longer valid. We need
            // to drop references to it and its offer, if any.
            Finished {} => {
                let Some(seat) = state.seats.get_mut(seat) else {
                    return;
                };
                if let Offer(ref offer) = seat.clipboard_state {
                    state.offers.remove(offer);
                }
                seat.set_device(None);
                seat.set_clipboard_state(Uninitialized)
            }
            _ => {}
        }
    }

    event_created_child!(Clipboard, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// A [`ZwlrDataControlSource`](ZwlrDataControlSourceV1) represents data that another client can
/// request to paste. It receives
/// * The [`Send`](zwlr_data_control_source_v1::Event::Send) event when another client wants to paste
/// * The [`Cancelled`](zwlr_data_control_source_v1::Event::Cancelled) event when another client has
///   taken over the clipboard (i.e., someone else has copied).
impl Dispatch<ZwlrDataControlSourceV1, WlSeat> for Clipboard {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: <ZwlrDataControlSourceV1 as Proxy>::Event,
        seat: &WlSeat,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event::*;
        match event {
            // This event is received when someone wants to paste our data. We are provided with a
            // file descriptor and simply write the data to it.
            // Malformed Mime types are ignored!
            Send { mime_type, fd } => {
                if let Ok(m) = MimeType::from_string(mime_type) {
                    state.send(seat, m, fd);
                }
            }
            // This event is received when someone else has copied to indicate that we now don't own the
            // clipboard. We need to grab the clipboard back and receive the new newly copied data.
            Cancelled {} => {
                source.destroy();
                if let Some(data) = state.seats.get_mut(seat) {
                    data.set_clipboard_state(ClipboardState::Uninitialized);
                }
            }
            _ => (),
        }
    }
}

/// A [`ZwlrDataControlOffer`](ZwlrDataControlOfferV1) represents data that we can request from
/// another client, i.e., something that we can paste. It receives
/// [`Offer`](zwlr_data_control_offer_v1::Event::Offer) events to notify us of which MIME types are
/// available.
impl Dispatch<ZwlrDataControlOfferV1, ()> for Clipboard {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: <ZwlrDataControlOfferV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        use zwlr_data_control_offer_v1::Event::*;
        #[allow(clippy::single_match)]
        match event {
            Offer { mime_type } => {
                // We avoid registering offers that we created ourselves
                let Some(pipes) = state.offers.get_mut(offer) else {
                    return;
                };
                // Create a new pipe through which we can receive the data for this MIME type, then
                // request that data.
                match pipe() {
                    Ok((reader, writer)) => {
                        offer.receive(mime_type.clone(), writer.as_fd());
                        if let Ok(m) = MimeType::from_string(mime_type) {
                            pipes.insert(m, reader);
                        }
                    }
                    Err(e) => error!("Could not create pipe to receive data: {e}"),
                }
            }
            _ => {}
        }
    }
}
