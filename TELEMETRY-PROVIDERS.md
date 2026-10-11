# Telemetry providers in alpha.2

The relay client now uses one structured telemetry result for each source: provider, Roblox username/UserId when independently available, XYZ, sample time, validity, server identity when verified, and a failure reason. Only fresh, valid results may reach the room. Simulated data is accepted only in simulated rooms; an invalid real source never falls back to simulation. The fixed 60-stud attenuation and invalid/stale muting remain unchanged.

| Provider | Alpha.2 status | Trust boundary |
| --- | --- | --- |
| Read-only Roblox process reader | Implemented, but incompatible with the Roblox build checked on 2026-10-12 | Queries and reads only. Requires RTTI, exact Player/character match, and agreeing position copies. Unsupported builds publish no XYZ. |
| Cooperating experience | Interface reserved; no live feed configured or advertised as working | Must be installed by an authorized experience developer, authenticate server-originated samples, bind UserId and server instance, and enforce freshness. |
| Simulation | Working for testing | Never substitutes for a failed real provider in an experimental Roblox room. |

Roblox documents [`Players.LocalPlayer`](https://create.roblox.com/docs/reference/engine/classes/Players) and [LocalScripts](https://create.roblox.com/docs/reference/engine/classes/LocalScript) for client-side position access. However, [HttpService](https://create.roblox.com/docs/cloud-services/http-service) generic HTTP requests are made by experience **server scripts**, not ordinary LocalScripts. A cooperating experience therefore needs its developer's consent, server-side handling of player positions, and an authenticated externally reachable HTTPS destination. It cannot simply POST to a user's localhost desktop service. Roblox HTTP request limits and server egress policy also apply. WolfLink does not currently run or require such an endpoint, and it never installs scripts into third-party experiences.

No documented universal desktop XYZ API for ordinary third-party Roblox experiences was found. The memory reader remains an explicitly version-dependent experimental provider, not a supported guarantee. The current client failed all five previously validated RTTI checks; no new offsets were guessed. Because JobId is not independently verified, strict production Roblox sessions remain disabled. The explicitly acknowledged **Unverified Roblox Server** room mode does not weaken local-player identity or position validation; if the reader is unsupported, audio remains muted.
