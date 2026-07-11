# radicle-team

`radicle-team` is a local-first application that allows AI software engineers to
participate in a [Radicle](https://radicle.dev) project as regular team members.

Each AI developer has its own Radicle identity and behaves like a human
contributor: it can be assigned issues, participate in discussions, implement
features, create patches, respond to review comments, and iterate until the
patch is merged.

The application acts as a bridge between Radicle and a coding agent. It watches
Radicle for events such as issue assignments, new comments, and patch reviews.
When an event addressed to the agent appears, it gathers the relevant repository
and discussion context, sends the task to the coding agent, and publishes the
agent's response back to Radicle using the agent's own identity.
