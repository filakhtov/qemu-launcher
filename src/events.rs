use std::{any::TypeId, collections::HashMap, mem::transmute};

pub trait Event<EventData: ?Sized>: 'static {}

pub trait EventEmitter {
    fn trigger<E: Event<X>, X: ?Sized>(&self, event: &E) {
        if let Some(listeners) = self.listeners::<E, X>() {
            for listener in listeners {
                let listener: &Box<dyn Fn(&E)> = unsafe { transmute(listener) };
                (*listener)(event);
            }
        }
    }

    fn listeners<E: Event<X>, X: ?Sized>(&self) -> Option<&Vec<Box<dyn Fn(&())>>>;
}

pub struct EventDispatcher {
    listeners: HashMap<TypeId, Vec<Box<dyn Fn(&())>>>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        Self {
            listeners: HashMap::new(),
        }
    }

    pub fn add_listener<E: Event<X>, X: ?Sized, F: Fn(&E)>(&mut self, listener: F) {
        let callback: Box<dyn Fn(&E)> = Box::new(listener);
        let callback: Box<dyn Fn(&())> = unsafe { transmute(callback) };

        self.listeners
            .entry(TypeId::of::<E>())
            .or_insert(vec![])
            .push(callback);
    }
}

impl EventEmitter for EventDispatcher {
    fn listeners<E: Event<X>, X: ?Sized>(&self) -> Option<&Vec<Box<dyn Fn(&())>>> {
        return self.listeners.get(&TypeId::of::<E>());
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
        static mut TEST_EVENT_COUNT: u8 = 0;

        event_dispatcher.add_listener(|_event: &TestEvent| unsafe {
            TEST_EVENT_COUNT += 1;
        });

        event_dispatcher.trigger(&TestEvent {});
        event_dispatcher.trigger(&EventWithNoListeners {});

        unsafe {
            assert_eq!(1, TEST_EVENT_COUNT);
        }
    }
}
