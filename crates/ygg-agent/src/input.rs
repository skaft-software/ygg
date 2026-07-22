//! Typed user input crossing the agent boundary: ordered text and media parts.

use ygg_ai::{Media, UserPart};

/// A user-authored input: ordered text and media parts.
///
/// This is the type accepted by [`Agent::prompt`](crate::Agent::prompt),
/// [`RunControl::steer`](crate::RunControl::steer) and
/// [`RunControl::follow_up`](crate::RunControl::follow_up). Plain strings
/// convert via `From`, so text-only callers pass `&str`/`String` unchanged.
#[derive(Clone, Debug)]
pub struct UserInput {
    /// Ordered content parts.
    pub parts: Vec<InputPart>,
}

/// One part of a [`UserInput`].
#[derive(Clone, Debug)]
pub enum InputPart {
    /// Plain text.
    Text(String),
    /// Image or audio payload.
    Media(Media),
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            parts: vec![InputPart::Text(text)],
        }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<Vec<InputPart>> for UserInput {
    fn from(parts: Vec<InputPart>) -> Self {
        Self { parts }
    }
}

impl UserInput {
    /// Human-readable single-line summary: text parts joined, media parts as
    /// `[image]` / `[audio]`. Used for steering-delivery events and logs.
    pub fn text_summary(&self) -> String {
        let mut pieces = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            match part {
                InputPart::Text(text) => pieces.push(text.clone()),
                InputPart::Media(Media::Image(_)) => pieces.push("[image]".into()),
                InputPart::Media(Media::Audio(_)) => pieces.push("[audio]".into()),
            }
        }
        pieces.join(" ")
    }

    /// Converts the parts into session-persistable [`UserPart`]s, 1:1.
    pub fn into_user_parts(self) -> Vec<UserPart> {
        self.parts
            .into_iter()
            .map(|part| match part {
                InputPart::Text(text) => UserPart::Text(text),
                InputPart::Media(media) => UserPart::Media(media),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_media() -> Media {
        Media::image_bytes(
            bytes::Bytes::from_static(&[0x89, 0x50]),
            "image/png".parse().unwrap(),
        )
    }

    #[test]
    fn from_string_yields_one_text_part() {
        let input = UserInput::from("hello".to_owned());
        assert!(matches!(&input.parts[..], [InputPart::Text(t)] if t == "hello"));
    }

    #[test]
    fn text_summary_joins_text_and_labels_media() {
        let input = UserInput::from(vec![
            InputPart::Text("look at".into()),
            InputPart::Media(png_media()),
            InputPart::Text("please".into()),
        ]);
        assert_eq!(input.text_summary(), "look at [image] please");
    }

    #[test]
    fn into_user_parts_maps_one_to_one_preserving_order() {
        let input = UserInput::from(vec![
            InputPart::Text("a".into()),
            InputPart::Media(png_media()),
        ]);
        let parts = input.into_user_parts();
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], UserPart::Text(t) if t == "a"));
        assert!(matches!(&parts[1], UserPart::Media(Media::Image(_))));
    }
}
