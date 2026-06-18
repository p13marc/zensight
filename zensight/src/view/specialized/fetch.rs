//! A small state machine for on-demand fetches, so the UI can distinguish
//! "not asked yet" / "loading" / "loaded" / "failed" instead of a bare
//! `Option<T>` that makes a slow or failed fetch look identical to "no data".
//!
//! See docs/plans/gui/05-states-feedback-accessibility.md (L1).

/// The state of an on-demand fetch.
#[derive(Debug, Clone, Default)]
pub enum Fetch<T> {
    /// Not requested yet.
    #[default]
    Idle,
    /// Request in flight.
    Loading,
    /// Loaded successfully.
    Ready(T),
    /// Failed; carries a human-readable reason.
    Error(String),
}

impl<T> Fetch<T> {
    /// Is a request currently in flight?
    pub fn is_loading(&self) -> bool {
        matches!(self, Fetch::Loading)
    }

    /// The loaded value, if ready.
    pub fn ready(&self) -> Option<&T> {
        match self {
            Fetch::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// The error message, if failed.
    pub fn error(&self) -> Option<&str> {
        match self {
            Fetch::Error(message) => Some(message.as_str()),
            _ => None,
        }
    }

    /// Fold a fetch result into the appropriate terminal state.
    pub fn from_result(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Fetch::Ready(value),
            Err(message) => Fetch::Error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        let mut f: Fetch<Vec<u8>> = Fetch::default();
        assert!(matches!(f, Fetch::Idle));
        assert!(!f.is_loading());
        f = Fetch::Loading;
        assert!(f.is_loading());
        assert!(f.ready().is_none());
        f = Fetch::from_result(Ok(vec![1, 2, 3]));
        assert_eq!(f.ready().map(|v| v.len()), Some(3));
        f = Fetch::from_result(Err("boom".into()));
        assert_eq!(f.error(), Some("boom"));
        assert!(f.ready().is_none());
    }
}
