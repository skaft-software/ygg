//! Validated public identifiers.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

const MAX_ID_BYTES: usize = 128;

fn validate_id(kind: &'static str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(format!("{kind} exceeds the {MAX_ID_BYTES}-byte limit"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(format!("{kind} contains an invalid character"));
    }
    Ok(())
}

macro_rules! identifier {
    ($name:ident, $kind:literal) => {
        #[doc = concat!("Validated public ", $kind, ".")]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Constructs a validated ", $kind, ".")]
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                validate_id($kind, &value)?;
                Ok(Self(value))
            }

            /// Returns the identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(HostId, "host ID");
identifier!(DeviceId, "device ID");
identifier!(ProjectId, "project ID");
identifier!(SessionId, "session ID");
identifier!(RunId, "run ID");
identifier!(TurnId, "turn ID");
identifier!(ItemId, "item ID");
identifier!(DurableEntryId, "durable session entry ID");
identifier!(RequestId, "request ID");
identifier!(CommandId, "command ID");
identifier!(SourceId, "source ID");
identifier!(ArtifactId, "artifact ID");
identifier!(ThemeId, "theme ID");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_validate_during_deserialization() {
        let oversized = "x".repeat(MAX_ID_BYTES + 1);
        let json = serde_json::to_string(&oversized).unwrap();
        assert!(serde_json::from_str::<SessionId>(&json).is_err());
        assert!(serde_json::from_str::<SessionId>("\"session/escape\"").is_err());
        assert_eq!(
            serde_json::from_str::<SessionId>("\"session-01\"").unwrap(),
            SessionId::new("session-01").unwrap()
        );
    }
}
