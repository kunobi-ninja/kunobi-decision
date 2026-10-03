//! Typed choice labels.
//!
//! Declare the labels of a choice as a Rust enum with [`labels!`](macro@crate::labels),
//! ask with [`choice_of`](crate::choice_of), and read back a
//! [`TypedChoiceAnswer`](crate::TypedChoiceAnswer) whose `choice` is that enum.

/// A fixed set of choice labels, usually declared with [`labels!`](macro@crate::labels).
pub trait Labels: Sized + Copy + Eq + 'static {
    /// Every label in the order the model sees them: variant, wire label, optional description.
    const ALL: &'static [(Self, &'static str, Option<&'static str>)];

    /// The wire label of this variant.
    fn label(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(variant, _, _)| *variant == self)
            .map(|(_, label, _)| *label)
            .expect("Labels::ALL must list every variant")
    }

    /// The variant for a wire label.
    fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, candidate, _)| *candidate == label)
            .map(|(variant, _, _)| *variant)
    }
}

/// Declare an enum of choice labels.
///
/// Each variant names its wire label and, optionally, a description the model
/// sees. The macro derives `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash` and
/// `Debug`; don't derive those again. `#[cfg]` on the enum or its variants is not
/// supported, because the generated `Labels` impl lists every variant.
///
/// ```
/// kunobi_decision::labels! {
///     /// Who should handle a ticket.
///     pub enum Team {
///         Billing = "billing": "Payment or subscription issues",
///         Technical = "technical": "Bugs or integration problems",
///         Other = "other",
///     }
/// }
///
/// use kunobi_decision::Labels;
/// assert_eq!(Team::Billing.label(), "billing");
/// assert_eq!(Team::from_label("other"), Some(Team::Other));
/// ```
#[macro_export]
macro_rules! labels {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident = $label:literal $(: $description:literal)?
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $crate::Labels for $name {
            const ALL: &'static [(Self, &'static str, ::core::option::Option<&'static str>)] = &[
                $(
                    ($name::$variant, $label, $crate::__label_description!($($description)?)),
                )+
            ];
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __label_description {
    () => {
        ::core::option::Option::None
    };
    ($description:literal) => {
        ::core::option::Option::Some($description)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::labels! {
        enum Tone {
            Calm = "calm",
            Angry = "angry": "Strong language",
        }
    }

    #[test]
    fn labels_round_trip() {
        assert_eq!(Tone::Calm.label(), "calm");
        assert_eq!(Tone::from_label("angry"), Some(Tone::Angry));
        assert_eq!(Tone::from_label("Calm"), None);
        assert_eq!(Tone::ALL[1].2, Some("Strong language"));
        assert_eq!(Tone::ALL[0].2, None);
    }
}
