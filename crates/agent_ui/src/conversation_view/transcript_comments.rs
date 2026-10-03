//! Comments on passages of the transcript editor, and the text format that carries them to
//! the agent. Sent comments live in the user message that carried them, so they survive
//! reloading a thread without separate storage.

const COMMENTS_START: &str = "<comments>";
const COMMENTS_END: &str = "</comments>";
const REPLY_START: &str = "<comment-reply id=\"";
const REPLY_END: &str = "</comment-reply>";

/// A comment the user wrote on a passage, as carried in a user message.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CommentText {
    pub id: u32,
    pub quote: String,
    pub text: String,
}

/// The agent's answer to a comment, taken out of its prose.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CommentReply {
    pub id: u32,
    pub text: String,
    /// False while the reply is still streaming in.
    pub complete: bool,
}

/// The block appended to a user message to send comments with it.
pub(super) fn comments_message(comments: &[CommentText]) -> String {
    let mut message = format!(
        "{COMMENTS_START}\n\
         The user commented on earlier passages of this conversation. Answer each comment in \
         a block placed where the answer belongs in your reply, quoting its id:\n\
         {REPLY_START}ID\">your answer{REPLY_END}\n\
         Text outside these blocks is your normal reply.\n"
    );
    for comment in comments {
        message.push_str(&format!(
            "\n<comment id=\"{}\">\n<quote>{}</quote>\n<text>{}</text>\n</comment>\n",
            comment.id, comment.quote, comment.text
        ));
    }
    message.push_str(COMMENTS_END);
    message
}

/// Splits a user message into the text the user typed and the comments sent with it.
pub(super) fn split_user_message(text: &str) -> (&str, Vec<CommentText>) {
    let Some(start) = text.find(COMMENTS_START) else {
        return (text, Vec::new());
    };
    let block = &text[start..];
    let block = block.find(COMMENTS_END).map_or(block, |end| &block[..end]);
    let mut comments = Vec::new();
    let mut rest = block;
    while let Some(comment_start) = rest.find("<comment id=\"") {
        rest = &rest[comment_start + "<comment id=\"".len()..];
        let Some(id_end) = rest.find('"') else {
            break;
        };
        let id = rest[..id_end].parse().ok();
        let body_end = rest.find("</comment>").unwrap_or(rest.len());
        let body = &rest[..body_end];
        rest = &rest[body_end..];
        let (Some(id), Some(quote), Some(text)) = (
            id,
            between(body, "<quote>", "</quote>"),
            between(body, "<text>", "</text>"),
        ) else {
            continue;
        };
        comments.push(CommentText {
            id,
            quote: quote.to_string(),
            text: text.to_string(),
        });
    }
    (text[..start].trim_end(), comments)
}

/// Takes the agent's comment replies out of its prose. A reply that is still streaming is
/// cut off at its start, so its text never flashes in the prose.
pub(super) fn extract_replies(prose: &str) -> (String, Vec<CommentReply>) {
    let mut remaining = String::with_capacity(prose.len());
    let mut replies = Vec::new();
    let mut rest = prose;
    while let Some(start) = rest.find(REPLY_START) {
        remaining.push_str(&rest[..start]);
        let after_start = &rest[start + REPLY_START.len()..];
        let Some(id_end) = after_start.find('"') else {
            rest = "";
            break;
        };
        let id = after_start[..id_end].parse::<u32>().ok();
        let after_id = &after_start[id_end..];
        let Some(open_end) = after_id.find('>') else {
            rest = "";
            break;
        };
        let body = &after_id[open_end + 1..];
        let (text, complete, next) = match body.find(REPLY_END) {
            Some(end) => (&body[..end], true, &body[end + REPLY_END.len()..]),
            None => (body, false, ""),
        };
        if let Some(id) = id {
            replies.push(CommentReply {
                id,
                text: text.trim().to_string(),
                complete,
            });
        }
        rest = next;
    }
    remaining.push_str(rest);
    (collapse_blank_lines(&remaining), replies)
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(text[from..to].trim())
}

/// Removing a reply block leaves the blank lines around it behind.
fn collapse_blank_lines(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut newlines = 0;
    for character in text.trim().chars() {
        if character == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        collapsed.push(character);
    }
    collapsed
}
