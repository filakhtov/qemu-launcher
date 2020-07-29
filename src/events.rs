use std::{any::TypeId, collections::HashMap, mem::transmute};

pub trait Event<EventData: ?Sized>: 'static {}

pub trait EventEmitter {
    fn trigger<E: Event<X>, X: ?Sized>(&mut self, event: &E);
}

pub struct EventDispatcher {
    listeners: HashMap<TypeId, Vec<Box<dyn FnMut(&())>>>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        Self {
            listeners: HashMap::new(),
        }
    }

    pub fn add_listener<E: Event<X>, X: ?Sized, F: FnMut(&E)>(&mut self, listener: F) {
        let callback: Box<dyn FnMut(&E)> = Box::new(listener);
        let callback: Box<dyn FnMut(&())> = unsafe { transmute(callback) };

        self.listeners
            .entry(TypeId::of::<E>())
            .or_insert(vec![])
            .push(callback);
    }
}

impl EventEmitter for EventDispatcher {
    fn trigger<E: Event<X>, X: ?Sized>(&mut self, event: &E) {
        if let Some(listeners) = self.listeners.remove(&TypeId::of::<E>()) {
            for listener in listeners {
                let mut listener: Box<dyn FnMut(&E)> = unsafe { transmute(listener) };
                listener(event);
                self.add_listener(listener);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{Event, EventDispatcher, EventEmitter};

    struct TestEvent {}
    impl Event<TestEvent> for TestEvent {}

    struct EventWithNoListeners {}
    impl Event<EventWithNoListeners> for EventWithNoListeners {}

    #[test]
    fn event_dispatcher_notifies_appropriate_event_listeners() {
        let mut event_dispatcher = EventDispatcher::new();
        let mut test_event_count = 0;

        event_dispatcher.add_listener(|_event: &TestEvent| {
            test_event_count += 1;
        });

        event_dispatcher.trigger(&TestEvent {});
        event_dispatcher.trigger(&EventWithNoListeners {});

        assert_eq!(1, test_event_count);
    }
}
