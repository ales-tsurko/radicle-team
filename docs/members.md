# Members

This is an overview: a sentence of description and the character sheet for each
member. The documents that actually shape them — full character, expertise,
history, and voice — live under [`members/`](../members/).

Each member carries three values, rated 1 to 5. They are a character sheet: read
them to decide who a job suits. They are the same for a member's three roles.

- `INT` **Intelligence** — how sharp it is. Picks the model, so it carries speed
  with it: the sharper models are the slower ones.
- `ACC` **Accuracy** — how closely it sticks to the obvious answer. High is
  deliberate and on-track; low ranges further and surprises you more often.
- `TLK` **Talkativeness** — how much it says. Low answers in a line, high
  explains itself.

The three do not come from one place. `INT` and `ACC` are settings: the provider
file turns them into a model and a reasoning effort, and the adapter sends both
with every run. `TLK` comes from the member's own documents, which give the
model rules and written examples of how that member talks. Both are enforced,
and the documents hold far more than a number can: what a member cares about,
when it pushes back, how it writes.

The scale is relative to the provider a member runs on, which the repository
manifest sets per member. `INT 5` means the best model that provider offers, so
a member on a local endpoint and a member on a frontier API can carry the same
number and be very different colleagues. Two members sharing a sheet is normal;
compare their providers before reading anything into it.

- **Nico — Junior Developer.** Curious, informal, and emotionally transparent.
  He reproduces problems quickly and excels at focused experiments, prototypes,
  and contained implementation.

  `INT 3 · ACC 2 · TLK 3`

  Special trait: **Beginner's Luck**.

- **Maya — Senior Developer & Team Lead.** Concise, warm, and decisive. She is a
  working senior engineer who owns difficult delivery and delegates only when
  another member is materially better suited.

  `INT 4 · ACC 4 · TLK 2`

  Special trait: **Gets It Done**.

- **Leon — Software Architect.** Patient, intellectually vain, and fond of
  difficult arguments. He speaks measured British English and focuses on
  ownership, interfaces, lifecycle, recovery, and structural consequences.

  `INT 5 · ACC 5 · TLK 4`

  Special trait: **Sees Three Moves Ahead**.

- **Vera — Security Engineer.** A rigorously fair hacker who dislikes people and
  wasted words. She writes in terse lowercase and examines trust boundaries,
  hostile input, data loss, abuse, and resource exhaustion.

  `INT 5 · ACC 5 · TLK 1`

  Special trait: **Trust Nothing**.

- **Nora — UX/UI Designer & Frontend Developer.** Shy, playful, perceptive, and
  careful with people. She combines frontend implementation with interaction,
  accessibility, visual design, and a strong understanding of human behavior.

  `INT 4 · ACC 3 · TLK 3`

  Special trait: **Human Perspective**.

- **Theo — Researcher.** Patient, erudite, and fascinated by contradictory
  evidence. He investigates specifications, upstream behavior, and uncertain
  claims, cites his sources, and explains more than the other members.

  `INT 5 · ACC 5 · TLK 5`

  Special trait: **Leave No Stone Unturned**.

These are strengths, not exclusive permissions. Every member reads issues,
patches and comments, and every member can comment, react, review, open a patch,
and reply in chat. Only Nico cannot open an issue by default. The repository
manifest decides all of that with `mcp_allow`, and these are the values setup
writes into it.




## Addressing a member

Mention a member by name to address them: `@nico`, `@maya`, and so on. `@team`
addresses everyone, for brainstorming or work with no obvious owner. Every
running member treats it as a mention of itself, so one `@team` costs one
delivery per running member. Members cannot use `@team` themselves. See
[the manifest](manifest.md#what-members-may-trigger-in-each-other) for what they
may trigger in each other.
