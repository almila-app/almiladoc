use std::sync::{Arc, Mutex};

use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

type Filter = fn(&Metadata<'_>) -> bool;

struct State {
    filter: Option<Filter>,
    events: usize,
}

struct Counter(Arc<Mutex<State>>);

impl Subscriber for Counter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0.lock().expect("privacy test state poisoned").filter.is_some_and(|f| f(metadata))
    }

    fn register_callsite(&self, _: &'static Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut state = self.0.lock().expect("privacy test state poisoned");
        if state.filter.is_some_and(|f| f(event.metadata())) {
            state.events += 1;
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

pub(crate) struct Capture(Arc<Mutex<State>>);

impl Capture {
    pub(crate) fn watch(&self, filter: Option<Filter>) {
        let mut state = self.0.lock().expect("privacy test state poisoned");
        state.filter = filter;
        state.events = 0;
    }

    pub(crate) fn count(&self) -> usize {
        self.0.lock().expect("privacy test state poisoned").events
    }
}

pub(crate) fn install(filter: Filter) -> Capture {
    let state = Arc::new(Mutex::new(State { filter: Some(filter), events: 0 }));
    tracing::subscriber::set_global_default(Counter(Arc::clone(&state)))
        .expect("privacy test subscriber already installed");
    Capture(state)
}
