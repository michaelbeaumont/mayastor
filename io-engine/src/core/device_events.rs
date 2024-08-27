use std::{
    fmt::Debug,
    ops::Deref,
    sync::{Arc, Weak},
};

use parking_lot::Mutex;

/// TODO
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum DeviceEventType {
    /// Device has been removed.
    DeviceRemoved,
    /// Special case for loopback device removal: loopback devices are not
    /// real SPDK bdevs but bdev aliases. Deleting an alias won't make
    /// SPDK send a proper bdev remove event.
    LoopbackRemoved,
    /// Device has been resized.
    DeviceResized,
    /// TODO
    MediaManagement,
    /// Sent when admin q polling fails for the first time.
    AdminCommandCompletionFailed,
    /// When the adminq poll fails the first time, the controller may not yet
    /// be failed.
    /// Next time the admin q poll fails, if the controller is noticed as
    /// failed for the first time, this event is sent, allowing further
    /// clean up to be performed.
    AdminQNoticeCtrlFailed,
}

/// TODO
pub trait DeviceEventListener {
    /// TODO
    fn handle_device_event(&self, evt: DeviceEventType, dev_name: &str);

    /// TODO
    fn get_listener_name(&self) -> String {
        "unnamed device event listener".to_string()
    }
}

/// TODO
struct DeviceEventListenerRef(&'static dyn DeviceEventListener);

unsafe impl Send for DeviceEventListenerRef {}

impl Deref for DeviceEventListenerRef {
    type Target = dyn DeviceEventListener;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// TODO
struct SinkInner {
    cell: Mutex<DeviceEventListenerRef>,
}

impl SinkInner {
    /// TODO
    fn new(p: &dyn DeviceEventListener) -> Self {
        let p = unsafe {
            std::mem::transmute::<
                &dyn DeviceEventListener,
                &'static dyn DeviceEventListener,
            >(p)
        };

        Self {
            cell: Mutex::new(DeviceEventListenerRef(p)),
        }
    }

    /// TODO
    fn dispatch_event(&self, evt: DeviceEventType, dev_name: &str) {
        self.cell.lock().handle_device_event(evt, dev_name);
    }

    /// TODO
    fn get_listener_name(&self) -> String {
        self.cell.lock().get_listener_name()
    }
}

/// A reference for a device event listener.
/// This object behaves like a reference counted reference to dispatcher's inner
/// representation of event listener instance.
#[derive(Clone)]
pub struct DeviceEventSink {
    inner: Arc<SinkInner>,
}

impl DeviceEventSink {
    /// TODO
    pub fn new(lst: &dyn DeviceEventListener) -> Self {
        Self {
            inner: Arc::new(SinkInner::new(lst)),
        }
    }

    /// Consumes a event listener reference and returns a weak listener
    /// reference.
    fn into_weak(self) -> Weak<SinkInner> {
        Arc::downgrade(&self.inner)
    }

    /// TODO
    pub fn get_listener_name(&self) -> String {
        self.inner.get_listener_name()
    }
}

/// TODO
#[derive(Default)]
pub struct DeviceEventDispatcher {
    listeners: Mutex<Vec<Weak<SinkInner>>>,
}

impl DeviceEventDispatcher {
    /// Creates a new instance of device event dispatcher.
    pub fn new() -> Self {
        Default::default()
    }

    /// Adds a new event listener reference.
    /// The client code must retain a clone for the added reference in order to
    /// keep events coming. As long as the client code drops the last
    /// reference to an event listener, the dispatcher stops delivering events
    /// to it.
    ///
    /// # Arguments
    ///
    /// * `listener`: Reference to an event listener.
    pub fn add_listener(&self, listener: DeviceEventSink) {
        self.listeners.lock().push(listener.into_weak());
        self.purge();
    }

    /// Dispatches an event to all registered listeners.
    /// Returns the number of listeners notified about target event.
    pub fn dispatch_event(
        &self,
        evt: DeviceEventType,
        dev_name: &str,
    ) -> usize {
        let mut listeners = Vec::new();

        // To avoid potential deadlocks we never call the listeners with the
        // mutex held, just find all suitable listeners and save them
        // for further invocation.
        self.listeners.lock().iter_mut().for_each(|dst| {
            if let Some(p) = dst.upgrade() {
                listeners.push(Arc::clone(&p));
            }
        });

        // Invoke all listeners once the mutex is dropped.
        let notified = {
            for l in &listeners {
                l.dispatch_event(evt, dev_name);
            }
            listeners.len()
        };
        self.purge();
        notified
    }

    /// Returns the number of registered listeners.
    pub fn count(&self) -> usize {
        self.listeners.lock().iter().fold(0, |acc, x| {
            if x.strong_count() > 0 {
                acc + 1
            } else {
                acc
            }
        })
    }

    /// Removes all dropped listeners.
    fn purge(&self) {
        self.listeners.lock().retain(|x| x.strong_count() > 0);
    }
}
