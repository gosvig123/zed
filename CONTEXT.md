# Tasks and Chat

Tasks and Chat are separate modules in this Zed workspace.

## Language

**Task**: A work item managed through the tasks CLI and shown in the Task Shark panel.

**Tick**: A scheduled Pi prompt that runs with a working directory and records its results.

**Chat**: The module for agent conversations and Ticks.

**Tick host**: The machine that owns a Tick's schedule, runs, and transcripts; Zed reaches remote Tick hosts over SSH.

**Thread layout**: How Chat draws a conversation, set by `agent.thread_layout`. `chat` uses message cards and one card per tool call; `document` shows user messages as prose marked by an avatar, folds activity into summary lines, puts the message editor at the end of the transcript with its controls in a strip below, and shows a message rail. `editor` shows the transcript in a transcript editor.

**Transcript editor**: In the `editor` thread layout, the whole transcript as one read-only text editor, so the caret, selections, find, and editor motions move across all messages. Activity runs are folds; edits, terminals, and other entries that need a click render as cards in editor blocks. Markdown markup is hidden and styled; tables and diagrams render as blocks; the composer is its last block. Typing in it, or moving down past its end, continues in the composer.

**Transcript comment**: A comment on a passage of the transcript editor. Drafts are sent with the next message inside a `<comments>` block, and the agent answers each in a `<comment-reply>` block, which the transcript shows under the passage. Sent comments live in the user message that carried them, so they survive reloading a thread.

**Subthread**: A thread started from a transcript comment. It begins with the parent conversation, the quoted passage, and the comment. Its link to the parent (parent thread and quoted passage) lives in the `thread_parents` table of the thread metadata store. The sidebar nests subthreads under their parent, folded by default, and the parent shows a reference card at the passage that opens the subthread.

**Message rail**: In the `document` thread layout, markers on the right edge of the transcript, one per user message, that jump to that message and highlight the one being read.

**Activity group**: In the `document` thread layout, a run of two or more reads, searches, commands, fetches, or thoughts shown as one expandable summary line. File changes, permission prompts, failed tool calls, and subagents never join one.

## Relationships

- **Ticks** belong to **Chat**, not to the Task Shark panel.
- A **Tick** can have multiple runs, each with a transcript.
- A **Tick** belongs to exactly one **Tick host**. Tick ids are unique only per host.
