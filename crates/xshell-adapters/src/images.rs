//! Where images go in a provider request.
//!
//! Both adapters share one layout so that an image reaches the model the same
//! way whichever provider serves it:
//!
//! - A user message carries its own images.
//! - Chat-completions `tool` messages accept only text, so images a tool
//!   produced are sent in a `user` message placed after the block of tool
//!   results, naming the call they came from. Images on any other non-user
//!   message follow it the same way.
//! - An image is never dropped silently. Without vision, or without its
//!   bytes, the model gets a short placeholder naming it instead.

use base64::Engine as _;
use std::borrow::Cow;
use xshell_core::{ChatMessage, ImageBytes, ImageRef, MessageRole};

/// One message as it will be sent.
pub(crate) struct Laid<'a> {
    /// The conversation message, or `None` for a `user` message added to
    /// carry images that could not travel on their own message.
    pub source: Option<&'a ChatMessage>,
    pub content: Cow<'a, str>,
    pub images: Vec<EncodedImage>,
}

impl Laid<'_> {
    pub fn role(&self) -> String {
        self.source.map_or_else(
            || MessageRole::User.to_string(),
            |message| message.role.to_string(),
        )
    }
}

/// An image ready for the wire.
pub(crate) struct EncodedImage {
    pub media_type: &'static str,
    pub base64: String,
}

impl EncodedImage {
    /// A `data:` URL, as OpenAI-compatible `image_url` parts take.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.base64)
    }
}

/// Lay `messages` out for a provider. `vision` says whether the model takes
/// images; `bytes` holds the images the engine could read.
pub(crate) fn lay_out<'a>(
    messages: &'a [ChatMessage],
    bytes: &ImageBytes,
    vision: bool,
) -> Vec<Laid<'a>> {
    let mut laid = Vec::with_capacity(messages.len());
    // Images from non-user messages, waiting for the end of the tool block.
    let mut pending: Vec<(String, Vec<EncodedImage>)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let mut content = Cow::Borrowed(message.content.as_str());
        let mut sendable = Vec::new();
        for image in &message.images {
            match bytes.get(image.sha256()).filter(|_| vision) {
                Some(data) => {
                    append_line(&mut content, &format!("[image: {}]", describe(image)));
                    sendable.push(EncodedImage {
                        media_type: image.media_type().as_str(),
                        base64: base64::engine::general_purpose::STANDARD.encode(data.as_slice()),
                    });
                }
                None => {
                    let why = if vision {
                        "not available"
                    } else {
                        "not shown: this model is not configured for images"
                    };
                    append_line(
                        &mut content,
                        &format!("[image: {}, {why}]", describe(image)),
                    );
                }
            }
        }
        if message.role == MessageRole::User {
            laid.push(Laid {
                source: Some(message),
                content,
                images: sendable,
            });
        } else {
            if !sendable.is_empty() {
                append_line(&mut content, "[the images follow in the next user message]");
                pending.push((introduce(message), sendable));
            }
            laid.push(Laid {
                source: Some(message),
                content,
                images: Vec::new(),
            });
        }
        // Tool results must stay together after the assistant message that
        // asked for them, so held images are sent once the block ends.
        let block_continues = message.role == MessageRole::Tool
            && messages
                .get(index + 1)
                .is_some_and(|next| next.role == MessageRole::Tool);
        if !pending.is_empty() && !block_continues {
            let mut text = String::new();
            let mut images = Vec::new();
            for (intro, batch) in pending.drain(..) {
                append_line_owned(&mut text, &intro);
                images.extend(batch);
            }
            laid.push(Laid {
                source: None,
                content: Cow::Owned(text),
                images,
            });
        }
    }
    laid
}

fn describe(image: &ImageRef) -> String {
    format!("{}, {}x{}", image.label(), image.width(), image.height())
}

fn introduce(message: &ChatMessage) -> String {
    match (&message.role, &message.tool_call_id) {
        (MessageRole::Tool, Some(call_id)) => format!(
            "Images from the {} call {call_id}:",
            message.tool_name.as_deref().unwrap_or("tool")
        ),
        (role, _) => format!("Images from the previous {role} message:"),
    }
}

fn append_line(content: &mut Cow<'_, str>, line: &str) {
    let text = content.to_mut();
    append_line_owned(text, line);
}

fn append_line_owned(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xshell_core::{ImageMediaType, ToolCall};

    pub(crate) fn image(byte: u8, label: &str) -> (ImageRef, Vec<u8>) {
        let data = vec![byte; 4];
        let sha = format!("{:064x}", byte);
        let image = ImageRef::new(sha, ImageMediaType::Png, 10, 20, 4, label).unwrap();
        (image, data)
    }

    fn bytes(images: &[(ImageRef, Vec<u8>)]) -> ImageBytes {
        images
            .iter()
            .map(|(image, data)| (image.sha256().to_owned(), Arc::new(data.clone())))
            .collect()
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "run_shell".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn user_images_travel_on_their_message() {
        let attached = image(1, "photo.png");
        let messages = vec![ChatMessage::user("look").with_images([attached.0.clone()])];
        let laid = lay_out(&messages, &bytes(&[attached]), true);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].images.len(), 1);
        assert_eq!(laid[0].content, "look\n[image: photo.png, 10x20]");
        assert!(
            laid[0].images[0]
                .data_url()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn tool_images_follow_the_whole_block_of_tool_results() {
        let first = image(1, "render-a.png");
        let second = image(2, "render-b.png");
        let messages = vec![
            ChatMessage::user("render both"),
            ChatMessage::assistant_with_tools("", vec![call("a"), call("b")]),
            ChatMessage::tool_result(&call("a"), "rendered").with_images([first.0.clone()]),
            ChatMessage::tool_result(&call("b"), "rendered").with_images([second.0.clone()]),
            ChatMessage::assistant("done"),
        ];
        let laid = lay_out(&messages, &bytes(&[first, second]), true);
        let roles: Vec<_> = laid.iter().map(Laid::role).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "tool", "user", "assistant"]
        );
        assert!(laid[2].images.is_empty() && laid[3].images.is_empty());
        assert!(
            laid[2]
                .content
                .ends_with("[the images follow in the next user message]")
        );
        let carrier = &laid[4];
        assert!(carrier.source.is_none());
        assert_eq!(carrier.images.len(), 2);
        assert_eq!(
            carrier.content,
            "Images from the run_shell call a:\nImages from the run_shell call b:"
        );
    }

    #[test]
    fn without_vision_or_bytes_images_become_placeholders() {
        let attached = image(1, "photo.png");
        let messages = vec![ChatMessage::user("look").with_images([attached.0.clone()])];
        let blind = lay_out(&messages, &bytes(&[attached]), false);
        assert!(blind[0].images.is_empty());
        assert_eq!(
            blind[0].content,
            "look\n[image: photo.png, 10x20, not shown: this model is not configured for images]"
        );
        let missing = lay_out(&messages, &ImageBytes::new(), true);
        assert!(missing[0].images.is_empty());
        assert!(missing[0].content.ends_with("not available]"));
    }
}
