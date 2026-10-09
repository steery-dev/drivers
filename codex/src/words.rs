//! Whose words a Codex message carries, and what a person's message carried beside them.
//!
//! Codex posts its own fragments as though the person had typed them: the project's
//! instructions, the environment, a shell command's record, notices of its own. Those are
//! the harness's, never a person's, and they open no turn when Codex rolls turns back. The
//! list here is Codex's own closed list, in its own shapes ([`contextual`]).

use std::path::Path;

use steery_sdk::wire::harness::Clip;

/// A fragment Codex posted as the person which the person did not write.
///
/// Most are marked: an opening and a closing tag, compared without regard to case once the
/// text either side is trimmed. One opens with a key the close echoes back. Three are
/// warnings recognised by how they open. One is a small XML document. And
/// `<goal_context>` is matched against the text as written rather than a trimmed copy,
/// which is Codex's own inconsistency.
pub fn contextual(text: &str) -> bool {
    const MARKED: [(&str, &str); 7] = [
        ("# AGENTS.md instructions", "</INSTRUCTIONS>"),
        ("<environment_context>", "</environment_context>"),
        ("<user_shell_command>", "</user_shell_command>"),
        ("<turn_aborted>", "</turn_aborted>"),
        ("<subagent_notification>", "</subagent_notification>"),
        ("<recommended_plugins>", "</recommended_plugins>"),
        // One skill's own instructions; the list of skills is a developer fragment.
        ("<skill>", "</skill>"),
    ];
    const WARNINGS: [&str; 2] = [
        "Warning: The maximum number of unified exec processes you can keep open is",
        "Warning: Your account was flagged for potentially high-risk cyber activity",
    ];
    let head = text.trim();
    MARKED.iter().any(|(open, close)| marked(head, open, close))
        || WARNINGS.iter().any(|open| head.starts_with(open))
        || (head.starts_with("Warning: apply_patch was requested via ")
            && head.ends_with("Use the apply_patch tool instead of exec_command."))
        || (text.starts_with("<goal_context>") && text.ends_with("</goal_context>"))
        || keyed(head, "<external_", "</external_")
        || internal(head)
        || hooked(head)
}

/// Opens with one tag and closes with the other, whatever case either is written in.
fn marked(head: &str, open: &str, close: &str) -> bool {
    head.len() >= open.len() + close.len()
        && head
            .get(..open.len())
            .is_some_and(|at| at.eq_ignore_ascii_case(open))
        && head
            .get(head.len() - close.len()..)
            .is_some_and(|at| at.eq_ignore_ascii_case(close))
}

/// `<external_KEY>…</external_KEY>`: the close names the same key.
fn keyed(head: &str, open: &str, close: &str) -> bool {
    let Some(rest) = head.strip_prefix(open) else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let key = &rest[..end];
    !key.is_empty() && head.ends_with(&format!("{close}{key}>"))
}

/// `<codex_internal_context source="NAME">…</codex_internal_context>`, the name a
/// lower-case word.
fn internal(head: &str) -> bool {
    let Some(rest) = head.strip_prefix("<codex_internal_context source=\"") else {
        return false;
    };
    let Some(end) = rest.find('"') else {
        return false;
    };
    let mut name = rest[..end].chars();
    name.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && rest[end..].starts_with("\">")
        && head.ends_with("</codex_internal_context>")
}

/// `<hook_prompt hook_run_id="…">…</hook_prompt>`, which Codex parses as XML and accepts
/// only with a run id that is not blank: decoded the same way, so text merely resembling
/// one stays a person's.
fn hooked(head: &str) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(rename = "hook_prompt")]
    struct HookFragment {
        #[serde(rename = "@hook_run_id")]
        run_id: String,
        #[serde(rename = "$text")]
        _text: String,
    }
    head.starts_with("<hook_prompt")
        && quick_xml::de::from_str::<HookFragment>(head)
            .is_ok_and(|fragment| !fragment.run_id.trim().is_empty())
}

/// Whether one message begins a turn, exactly as Codex counts turns when it rolls them
/// back: a person's message unless any of its text parts is contextual; an agent's only
/// where it is one agent's message to another.
pub fn opens(person: bool, blocks: &[serde_json::Value]) -> bool {
    if person {
        !blocks
            .iter()
            .filter(|block| kind(block) == Some("input_text"))
            .any(|block| {
                block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(contextual)
            })
    } else {
        between_agents(blocks)
    }
}

fn kind(block: &serde_json::Value) -> Option<&str> {
    block.get("type").and_then(serde_json::Value::as_str)
}

/// The record one agent's message to another is, decoded rather than glanced at: four
/// required fields and every optional one of the right kind, the agents' paths validated
/// as Codex validates them. An agent merely answering in JSON does not decode.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct BetweenAgents {
    author: AgentPath,
    recipient: AgentPath,
    content: String,
    trigger_turn: bool,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    other_recipients: Vec<AgentPath>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    internal_chat_message_metadata_passthrough: Option<Passthrough>,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct Passthrough {
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    create_time: Option<serde_json::Number>,
    #[serde(default)]
    content_item_kinds: Option<serde::de::IgnoredAny>,
}

/// An agent's place among the others, in the only spellings Codex accepts: `/morpheus`,
/// or `/root` and lower-case names below it.
#[derive(serde::Deserialize)]
#[serde(try_from = "String")]
struct AgentPath;

impl TryFrom<String> for AgentPath {
    type Error = &'static str;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        if path == "/morpheus" {
            return Ok(Self);
        }
        let below = path
            .strip_prefix("/root")
            .ok_or("an agent is /morpheus or lives under /root")?;
        if below.is_empty() {
            return Ok(Self);
        }
        let below = below
            .strip_prefix('/')
            .ok_or("an agent below the root is separated from it")?;
        below
            .split('/')
            .all(|name| {
                !name.is_empty()
                    && name != "root"
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
            .then_some(Self)
            .ok_or("an agent's name is a lower-case word")
    }
}

/// Whether a message's one text part is the record of one agent's message to another.
pub fn between_agents(blocks: &[serde_json::Value]) -> bool {
    let [only] = blocks else { return false };
    if !matches!(kind(only), Some("input_text" | "output_text")) {
        return false;
    }
    only.get("text")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|text| serde_json::from_str::<BetweenAgents>(text).is_ok())
}

/// Whether this part is the `<image …>` part just before a picture the message carried or
/// the `</image>` part just after one: Codex's wrapping of the picture, not words.
pub fn wraps_picture(blocks: &[serde_json::Value], at: usize, text: &str) -> bool {
    let picture =
        |at: Option<usize>| at.and_then(|at| blocks.get(at)).and_then(kind) == Some("input_image");
    let head = text.trim();
    if head.eq_ignore_ascii_case("</image>") {
        return picture(at.checked_sub(1));
    }
    head.get(..7)
        .is_some_and(|open| open.eq_ignore_ascii_case("<image "))
        && head.ends_with('>')
        && !head.contains('\n')
        && picture(at.checked_add(1))
}

/// Whether the text is one element, `<name …>…</name>`, and nothing else: how a harness
/// wraps what it posts as the person (a Codex automation's `<heartbeat>`), never how a
/// person types.
pub fn enveloped(text: &str) -> bool {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let name = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
        .map_or(rest, |end| &rest[..end]);
    !name.is_empty()
        && rest[name.len()..].starts_with(|c: char| c == '>' || c.is_whitespace())
        && text
            .strip_suffix('>')
            .and_then(|text| text.strip_suffix(name))
            .is_some_and(|text| text.ends_with("</"))
}

/// The person's words in a message that carries attached files: what follows its
/// `## My request` line (`## My request:`, before that `## My request for Codex:`). The
/// whole text where there is none.
pub fn requested(text: &str) -> &str {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        at += line.len();
        if line.starts_with("## My request") && line.trim_end().ends_with(':') {
            return text[at..].trim();
        }
    }
    text
}

/// The pictures of one message on their way to their bytes: where each picture Codex
/// listed above the words sits in the clips until its bytes arrive, and the path the
/// `<image …>` part before the next picture gave it.
#[derive(Default)]
pub struct Pictures {
    listed: Vec<(usize, String)>,
    tagged: Option<String>,
}

/// One part of a person's message, for what it carried. Above their words Codex may write
/// the files they attached (`# Files mentioned by the user:`, each `## <name>: <path>`, a
/// picture among them `Image attachment: true`); each picture follows as an `input_image`
/// between an `<image name=… path="…">` part and an `</image>` part.
pub fn carried(block: &serde_json::Value, pictures: &mut Pictures, clips: &mut Vec<Clip>) {
    match kind(block) {
        Some("input_text") => {
            let Some(head) = block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
            else {
                return;
            };
            if let Some(tag) = head
                .strip_prefix("<image ")
                .and_then(|tag| tag.strip_suffix('>'))
            {
                pictures.tagged = attribute(tag, "path");
            } else if let Some(list) = head.strip_prefix("# Files mentioned by the user:") {
                files_mentioned(list, &mut pictures.listed, clips);
            }
        }
        Some("input_image") => {
            let Some(url) = block.get("image_url").and_then(serde_json::Value::as_str) else {
                return;
            };
            let path = pictures.tagged.take();
            let listed = match &path {
                Some(path) => pictures
                    .listed
                    .iter()
                    .position(|(_, listed)| listed == path),
                None => (!pictures.listed.is_empty()).then_some(0),
            };
            let clip = addressed(url, path.as_deref().and_then(last_segment));
            match (listed, clip) {
                // The bytes of a picture the list named: the listed clip gets them.
                (
                    Some(at),
                    Clip::Bytes {
                        data, media_type, ..
                    },
                ) => {
                    let (index, _) = pictures.listed.remove(at);
                    let name = match &clips[index] {
                        Clip::File { name, .. } | Clip::Path { name, .. } => name.clone(),
                        _ => None,
                    };
                    clips[index] = Clip::Bytes {
                        data,
                        media_type,
                        name,
                    };
                }
                (Some(at), Clip::File { .. }) => {
                    pictures.listed.remove(at);
                }
                (_, clip) => clips.push(clip),
            }
        }
        _ => {}
    }
}

/// Codex's list of a person's attached files, up to its `## My request` line.
fn files_mentioned(list: &str, listed: &mut Vec<(usize, String)>, clips: &mut Vec<Clip>) {
    let mut last: Option<usize> = None;
    for line in list.lines() {
        if line.starts_with("## My request") && line.trim_end().ends_with(':') {
            return;
        }
        let line = line.trim();
        if let Some(entry) = line.strip_prefix("## ") {
            // The path is absolute, so the name ends at the first `: /`.
            let (name, path) = match entry.find(": /") {
                Some(split) => (&entry[..split], &entry[split + 2..]),
                None => entry.rsplit_once(": ").unwrap_or(("", entry)),
            };
            let path = path.trim();
            if !path.starts_with('/') {
                last = None;
                continue;
            }
            last = Some(clips.len());
            clips.push(Clip::Path {
                path: path.into(),
                name: Some(name.trim())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
            });
        } else if line == "Image attachment: true"
            && let Some(index) = last.take()
            && let Clip::Path { path, name } = &clips[index]
        {
            let (path, name) = (path.clone(), name.clone());
            listed.push((index, path.to_string_lossy().into_owned()));
            clips[index] = Clip::File {
                name: name.or_else(|| last_segment(&path.to_string_lossy())),
                path,
                media_type: None,
            };
        }
    }
}

/// One attribute of a tag's inside, `name="value"`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let len = tag[start..].find('"')?;
    Some(tag[start..start + len].to_owned())
}

fn last_segment(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

/// A picture by the address Codex wrote: bytes in a `data:` URL, a file here, or a page.
fn addressed(url: &str, name: Option<String>) -> Clip {
    if let Some((head, data)) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        && let Some(media_type) = head.strip_suffix(";base64")
    {
        return Clip::Bytes {
            data: data.to_owned(),
            media_type: (!media_type.is_empty()).then(|| media_type.to_owned()),
            name,
        };
    }
    let local = url.strip_prefix("file://").unwrap_or(url);
    if local.starts_with('/') {
        return Clip::File {
            name: name.or_else(|| last_segment(local)),
            path: local.into(),
            media_type: None,
        };
    }
    Clip::Link {
        url: url.to_owned(),
    }
}

/// What a person asked for in one line, for a conversation Codex names nowhere: the first
/// line of their words, cut at a word within 120 characters.
pub fn asked_for(text: &str) -> Option<String> {
    const MOST: usize = 120;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.chars().count() <= MOST {
        return Some(line.to_owned());
    }
    let cut: String = line.chars().take(MOST - 1).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(at) if at > MOST / 2 => cut[..at].trim_end().to_owned(),
        _ => cut,
    };
    Some(format!("{cut}…"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Codex's own fragments, and text that merely resembles one, which stays a person's.
    #[test]
    fn codex_fragments_are_codex_s_and_look_alikes_are_the_person_s() {
        for fragment in [
            "# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>\nbe nice\n</INSTRUCTIONS>",
            "<environment_context>\n  <cwd>/w</cwd>\n</environment_context>",
            "<ENVIRONMENT_CONTEXT>x</environment_context>",
            "<user_shell_command>ls</user_shell_command>",
            "<turn_aborted>x</turn_aborted>",
            "<skill>\n<name>x</name>\n</skill>",
            "<external_notes>x</external_notes>",
            "<codex_internal_context source=\"memory_2\">x</codex_internal_context>",
            "<hook_prompt hook_run_id=\"r1\">keep going</hook_prompt>",
            "<goal_context>x</goal_context>",
            "Warning: The maximum number of unified exec processes you can keep open is 64.",
        ] {
            assert!(contextual(fragment), "{fragment}");
        }
        for person in [
            "please read <environment_context> for me",
            "<external_notes>x</external_other>",
            "<codex_internal_context source=\"Memory\">x</codex_internal_context>",
            "<hook_prompt hook_run_id=\" \">keep going</hook_prompt>",
            " <goal_context>x</goal_context>",
            "fix the bug",
        ] {
            assert!(!contextual(person), "{person}");
        }
    }

    /// One agent's message to another decodes as the record it is; an agent answering
    /// in JSON with the same field names does not.
    #[test]
    fn a_message_between_agents_is_decoded_not_glanced_at() {
        let record = json!({"author": "/root", "recipient": "/root/worker_1", "content": "go",
            "trigger_turn": true})
        .to_string();
        assert!(between_agents(&[
            json!({"type": "input_text", "text": record})
        ]));
        for text in [
            json!({"author": "me", "recipient": "/root", "content": "go", "trigger_turn": true}),
            json!({"author": "/root", "recipient": "/root/a/../b", "content": "go", "trigger_turn": true}),
            json!({"author": "/root", "recipient": "/root/x", "content": "go"}),
            json!({"author": "/root", "recipient": "/root/x", "content": "go", "trigger_turn": true, "other_recipients": null}),
        ] {
            assert!(
                !between_agents(&[json!({"type": "output_text", "text": text.to_string()})]),
                "{text}"
            );
        }
    }

    /// The files a person attached are clips, a picture with the bytes the message
    /// carried, and their words are what follows `## My request`.
    #[test]
    fn attached_files_are_clips_and_the_request_is_the_words() {
        let text = "# Files mentioned by the user:\n\n## shot.png: /tmp/shot.png\nImage attachment: true\n\n## notes.md: /w/notes.md\n\n## My request for Codex:\nlook at these";
        let blocks = [
            json!({"type": "input_text", "text": text}),
            json!({"type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/shot.png\">"}),
            json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA"}),
            json!({"type": "input_text", "text": "</image>"}),
        ];
        let mut pictures = Pictures::default();
        let mut clips = Vec::new();
        for block in &blocks {
            carried(block, &mut pictures, &mut clips);
        }
        assert_eq!(
            clips,
            vec![
                Clip::Bytes {
                    data: "AAAA".into(),
                    media_type: Some("image/png".into()),
                    name: Some("shot.png".into())
                },
                Clip::Path {
                    path: "/w/notes.md".into(),
                    name: Some("notes.md".into())
                },
            ]
        );
        assert_eq!(requested(text), "look at these");
        assert!(wraps_picture(
            &blocks,
            1,
            "<image name=[Image #1] path=\"/tmp/shot.png\">"
        ));
        assert!(wraps_picture(&blocks, 3, "</image>"));
        assert!(!wraps_picture(&blocks, 0, text));
    }

    #[test]
    fn a_harness_s_envelope_is_nobody_s_words() {
        assert!(enveloped("<heartbeat>tick</heartbeat>"));
        assert!(enveloped("<paseo-system kind=\"x\">a</paseo-system>"));
        assert!(!enveloped("<b>bold</b> and more"));
        assert!(!enveloped("plain words"));
    }
}
