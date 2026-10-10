//! [`Patch<T>`]: one field of a patch event, with `Option<Option<T>>` semantics
//! (unchanged | clear | set, architecture §4.2 notes).
//!
//! Fields that can be cleared use `Patch<T>`; fields that cannot (flags) use `Option<T>`
//! (unchanged | set). In patch structs both are written as
//! `#[serde(default, skip_serializing_if = "…")]`, so an absent field means "unchanged".

use serde::{Deserialize, Serialize};

/// Change to one optional field. Serialized with named variants: `"Unchanged"`, `"Clear"`,
/// `{"Set": value}`; inside patch structs an absent field is `Unchanged`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Patch<T> {
    /// Leave the field as it is.
    #[default]
    Unchanged,
    /// Set the field to "none".
    Clear,
    /// Set the field to this value.
    Set(T),
}

impl<T> Patch<T> {
    /// Whether this patch leaves the field alone (used by `skip_serializing_if`).
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }

    /// Applies the patch to `field`; returns whether the field was touched.
    pub fn apply(self, field: &mut Option<T>) -> bool {
        match self {
            Self::Unchanged => false,
            Self::Clear => {
                *field = None;
                true
            }
            Self::Set(v) => {
                *field = Some(v);
                true
            }
        }
    }

    /// Applies the patch to a clone of `current` and returns the result.
    pub fn applied(self, current: Option<T>) -> Option<T> {
        let mut field = current;
        self.apply(&mut field);
        field
    }

    /// `Patch<&T>` view.
    pub fn as_ref(&self) -> Patch<&T> {
        match self {
            Self::Unchanged => Patch::Unchanged,
            Self::Clear => Patch::Clear,
            Self::Set(v) => Patch::Set(v),
        }
    }

    /// Maps the value of a `Set`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Patch<U> {
        match self {
            Self::Unchanged => Patch::Unchanged,
            Self::Clear => Patch::Clear,
            Self::Set(v) => Patch::Set(f(v)),
        }
    }

    /// `None` = unchanged, `Some(None)` = clear, `Some(Some(v))` = set.
    pub fn into_option(self) -> Option<Option<T>> {
        match self {
            Self::Unchanged => None,
            Self::Clear => Some(None),
            Self::Set(v) => Some(Some(v)),
        }
    }
}

impl<T> From<Option<Option<T>>> for Patch<T> {
    fn from(v: Option<Option<T>>) -> Self {
        match v {
            None => Self::Unchanged,
            Some(None) => Self::Clear,
            Some(Some(v)) => Self::Set(v),
        }
    }
}

impl<T> From<Patch<T>> for Option<Option<T>> {
    fn from(p: Patch<T>) -> Self {
        p.into_option()
    }
}

/// Applies an `Option<T>` patch (unchanged | set) to a non-optional field; returns whether
/// the field was touched.
pub fn apply_set<T>(patch: Option<T>, field: &mut T) -> bool {
    match patch {
        Some(v) => {
            *field = v;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct P {
        #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
        name: Patch<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flag: Option<bool>,
    }

    #[test]
    fn three_states_apply() {
        let mut f = Some(1);
        assert!(!Patch::Unchanged.apply(&mut f));
        assert_eq!(f, Some(1));
        assert!(Patch::Set(2).apply(&mut f));
        assert_eq!(f, Some(2));
        assert!(Patch::<i32>::Clear.apply(&mut f));
        assert_eq!(f, None);
        assert_eq!(Patch::Set(3).applied(None), Some(3));
        let mut b = false;
        assert!(!apply_set(None, &mut b));
        assert!(apply_set(Some(true), &mut b) && b);
    }

    #[test]
    fn option_option_conversions() {
        for p in [Patch::Unchanged, Patch::Clear, Patch::Set(5u8)] {
            let oo: Option<Option<u8>> = p.into();
            assert_eq!(Patch::from(oo), p);
        }
        assert_eq!(Patch::Set(2).map(|v| v * 2), Patch::Set(4));
        assert_eq!(
            Patch::Set(String::from("x")).as_ref(),
            Patch::Set(&String::from("x"))
        );
    }

    #[test]
    fn absent_null_and_value_are_distinct_in_json() {
        let unchanged = P::default();
        let clear = P {
            name: Patch::Clear,
            flag: None,
        };
        let set = P {
            name: Patch::Set("Bob".into()),
            flag: Some(true),
        };
        assert_eq!(
            serde_json::to_string(&unchanged).ok().as_deref(),
            Some("{}")
        );
        assert_eq!(
            serde_json::to_string(&clear).ok().as_deref(),
            Some(r#"{"name":"Clear"}"#)
        );
        assert_eq!(
            serde_json::to_string(&set).ok().as_deref(),
            Some(r#"{"name":{"Set":"Bob"},"flag":true}"#)
        );
        for p in [unchanged, clear, set] {
            let json = serde_json::to_string(&p).expect("serialize");
            assert_eq!(serde_json::from_str::<P>(&json).ok(), Some(p));
        }
    }
}
