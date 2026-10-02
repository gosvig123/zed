# Tasks and Chat

Tasks and Chat are separate modules in this Zed workspace.

## Language

**Task**: A work item managed through the tasks CLI and shown in the Task Shark panel.

**Tick**: A scheduled Pi prompt that runs with a working directory and records its results.

**Chat**: The module for agent conversations and Ticks.

**Tick host**: The machine that owns a Tick's schedule, runs, and transcripts; Zed reaches remote Tick hosts over SSH.

## Relationships

- **Ticks** belong to **Chat**, not to the Task Shark panel.
- A **Tick** can have multiple runs, each with a transcript.
- A **Tick** belongs to exactly one **Tick host**. Tick ids are unique only per host.
