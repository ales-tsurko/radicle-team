# Members

| Member | Specialty | INT | ACC | TLK |
| --- | --- | --- | --- | --- |
| [Nico](../members/nico/CHARACTER.md) | Junior developer | 3 | 2 | 3 |
| [Maya](../members/maya/CHARACTER.md) | Senior developer / lead | 4 | 4 | 2 |
| [Leon](../members/leon/CHARACTER.md) | Software architect | 5 | 5 | 4 |
| [Vera](../members/vera/CHARACTER.md) | Security engineer | 5 | 5 | 1 |
| [Nora](../members/nora/CHARACTER.md) | UX/UI and frontend | 4 | 3 | 3 |
| [Theo](../members/theo/CHARACTER.md) | Researcher | 5 | 5 | 5 |

Scores range from 1 to 5:

- **INT** selects the model from the provider's ladder.
- **ACC** controls reasoning effort or temperature. Higher scores favor
  deliberation and consistency.
- **TLK** describes talkativeness, guided by the member's documents rather than
  a runtime length limit.

Scores are relative to a provider. An `INT 5` local model and an `INT 5` hosted
model need not have similar capabilities. Specialties guide task ownership,
while the [manifest](manifest.md) controls permissions.

Mention `@nico`, `@maya`, or another member by name. `@team` addresses every
running member and incurs a delivery for each. Members cannot use `@team`.

Full character, expertise, and voice documents live under
[`members/`](../members/).
