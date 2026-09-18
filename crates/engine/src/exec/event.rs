//! This module contains the set of compiler-emitted event codes, and their explanations
use miden_core::events::{EventId, EventName};
use miden_utils_sync::LazyLock;

/// This event indicates that a procedure call frame is entered
pub const FRAME_START_EVENT: EventName = EventName::new("readonly::miden_debug::frame_start");
static FRAME_START_EVENT_ID: LazyLock<EventId> = LazyLock::new(|| FRAME_START_EVENT.to_event_id());

/// This event indicates that a procedure call frame is exited
pub const FRAME_END_EVENT: EventName = EventName::new("readonly::miden_debug::frame_end");
static FRAME_END_EVENT_ID: LazyLock<EventId> = LazyLock::new(|| FRAME_END_EVENT.to_event_id());

/// This event indicates that a line should be printed.
///
/// The bytes representing the string are expected in memory. The executor reads the start address
/// and length from the operand stack.
///
/// The decoded string is emitted through the [`log`] infra at `Info` level on the `stdout`
/// target.
pub const PRINTLN_EVENT: EventName = EventName::new("readonly::miden_debug::println");
static PRINTLN_EVENT_ID: LazyLock<EventId> = LazyLock::new(|| PRINTLN_EVENT.to_event_id());

/// A typed wrapper around the raw trace events known to the compiler
#[derive(Debug, Clone)]
#[repr(u32)]
pub enum Event {
    FrameStart,
    FrameEnd,
    PrintLn,
    UserDefined(EventName),
    Unknown(EventId),
}

#[cfg(feature = "std")]
impl std::hash::Hash for Event {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_event_id().hash(state);
    }
}

impl Eq for Event {}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.as_event_id() == other.as_event_id()
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Event {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        if self.as_event_id() == other.as_event_id() {
            return Ordering::Equal;
        }
        match (self, other) {
            (Self::Unknown(a), Self::Unknown(b)) => a.cmp(b),
            (Self::Unknown(_), _) => Ordering::Greater,
            (_, Self::Unknown(_)) => Ordering::Less,
            (a, b) => a.as_event_name().unwrap().as_str().cmp(b.as_event_name().unwrap().as_str()),
        }
    }
}

impl Event {
    #[inline(always)]
    pub fn is_frame_start(&self) -> bool {
        matches!(self, Self::FrameStart)
    }

    #[inline(always)]
    pub fn is_frame_end(&self) -> bool {
        matches!(self, Self::FrameEnd)
    }

    pub fn as_event_id(&self) -> EventId {
        match self {
            Self::FrameStart => *FRAME_START_EVENT_ID,
            Self::FrameEnd => *FRAME_END_EVENT_ID,
            Self::PrintLn => *PRINTLN_EVENT_ID,
            Self::UserDefined(event) => event.to_event_id(),
            Self::Unknown(event) => *event,
        }
    }

    /// Get the [EventName] corresponding to this event
    ///
    /// Returns `None` if the name is unknown/requires lookup in the set of registered events
    pub fn as_event_name(&self) -> Option<EventName> {
        Some(match self {
            Self::FrameStart => FRAME_START_EVENT,
            Self::FrameEnd => FRAME_END_EVENT,
            Self::PrintLn => PRINTLN_EVENT,
            Self::UserDefined(name) => name.clone(),
            Self::Unknown(_) => return None,
        })
    }

    /// Returns `true` if `DebuggerHost` has a builtin handler for the event.
    pub fn has_builtin_handler(&self) -> bool {
        match self {
            Self::FrameStart | Self::FrameEnd | Self::PrintLn => true,
            Self::UserDefined(_) | Self::Unknown(_) => false,
        }
    }
}

impl core::fmt::Display for Event {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FrameStart => f.write_str(FRAME_START_EVENT.as_str()),
            Self::FrameEnd => f.write_str(FRAME_END_EVENT.as_str()),
            Self::PrintLn => f.write_str(PRINTLN_EVENT.as_str()),
            Self::UserDefined(name) => f.write_str(name.as_str()),
            Self::Unknown(id) => write!(f, "{id}"),
        }
    }
}

impl From<EventId> for Event {
    fn from(raw: EventId) -> Self {
        if raw == *FRAME_START_EVENT_ID {
            Self::FrameStart
        } else if raw == *FRAME_END_EVENT_ID {
            Self::FrameEnd
        } else if raw == *PRINTLN_EVENT_ID {
            Self::PrintLn
        } else {
            Self::Unknown(raw)
        }
    }
}

impl From<Event> for EventId {
    fn from(event: Event) -> Self {
        event.as_event_id()
    }
}

impl From<EventName> for Event {
    fn from(value: EventName) -> Self {
        if value == FRAME_START_EVENT {
            Self::FrameStart
        } else if value == FRAME_END_EVENT {
            Self::FrameEnd
        } else if value == PRINTLN_EVENT {
            Self::PrintLn
        } else {
            Self::UserDefined(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    #[cfg(feature = "std")]
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    use super::*;

    #[test]
    fn print_ln_event_roundtrips() {
        assert_eq!(Event::from(PRINTLN_EVENT.to_event_id()), Event::PrintLn);
        assert_eq!(Event::PrintLn.as_event_id(), *PRINTLN_EVENT_ID);
        assert_eq!(Event::from(PRINTLN_EVENT), Event::PrintLn);
    }

    #[test]
    fn event_metadata_covers_builtin_custom_and_unknown_events() {
        let custom_name = EventName::new("test::custom");
        let custom = Event::from(custom_name.clone());
        let unknown = Event::Unknown(EventId::from_u64(99));

        for event in [Event::FrameStart, Event::FrameEnd, Event::PrintLn] {
            assert!(event.as_event_name().is_some());
            assert!(event.has_builtin_handler());
            assert!(!event.to_string().is_empty());
        }
        assert!(custom.as_event_name().is_some());
        assert!(!custom.has_builtin_handler());
        assert_eq!(custom.to_string(), "test::custom");
        assert!(unknown.as_event_name().is_none());
        assert!(!unknown.has_builtin_handler());
        assert_eq!(unknown.to_string(), "99");

        assert!(Event::FrameStart.is_frame_start());
        assert!(!Event::FrameEnd.is_frame_start());
        assert!(Event::FrameEnd.is_frame_end());
        assert!(!Event::PrintLn.is_frame_end());
        assert_eq!(EventId::from(Event::FrameStart), *FRAME_START_EVENT_ID);
    }

    #[test]
    #[cfg(feature = "std")]
    fn events_order_by_name_with_unknown_values_last() {
        let custom = Event::from(EventName::new("test::custom"));
        let earlier = Event::from(EventName::new("test::alpha"));
        assert!(earlier < custom);
        assert!(custom > earlier);
        assert_eq!(custom.cmp(&custom), core::cmp::Ordering::Equal);
        let unknown = Event::Unknown(EventId::from_u64(1));
        assert!(Event::FrameStart < unknown);
        assert!(custom < unknown);
        assert_eq!(unknown, Event::Unknown(EventId::from_u64(1)));
        assert!(unknown > Event::FrameEnd);

        let mut first = DefaultHasher::new();
        unknown.hash(&mut first);
        let mut second = DefaultHasher::new();
        Event::Unknown(EventId::from_u64(1)).hash(&mut second);
        assert_eq!(first.finish(), second.finish());
    }
}
