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
