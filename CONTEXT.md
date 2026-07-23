# QQ Domain Language

## Terms

### Workspace

The codebase scope managed by a QQ server. A workspace contains root sessions
and their descendants.

### Session

A durable, focusable conversation in a workspace. A session may be a root or
have one parent session, can run independently of other sessions, and outlives
any attached client.

### Root Session

A session with no parent.

### Child Session

A session created in relation to one parent session. Child sessions retain
their own conversation and may run concurrently with their parent and siblings.

### Session Tree

A root session and all of its descendants.

### Prompt

User input submitted to one session. Prompts are ordered within that session.

### Follow-Up

A prompt submitted while its session already has an active run. It waits in
that session's queue and starts after earlier prompts finish.

### Run

The execution of one prompt within a session. A session has at most one active
run, while different sessions may have active runs concurrently.

### Client

An attached TUI, CLI, or future interface. Clients observe shared durable state
and may submit commands, but do not own sessions or runs.

### Threadline

A TUI view of a session tree and its concurrent activity. Threadline is not a
separate persisted domain object.

### Fold/Focus

A TUI view that condenses inactive history and emphasizes the focused session
and current activity. Fold/Focus uses the same session state as Threadline.
