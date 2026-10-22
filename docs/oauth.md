# OAuth: acting for a player

Arena is an OAuth 2 provider. Your application can ask a player to approve a
scoped, revocable access token that acts as them on the Arena API — with
exactly the permissions they saw on the approval screen, for exactly the
snakes they picked.

The flow is the standard **authorization code grant with PKCE** (RFC 6749 +
RFC 7636). `S256` is required; the `plain` method is rejected.

## Registering a client

Clients are admin-created only — there is no self-serve registration. Ask an
Arena admin and bring:

- your application's **name**, **logo URL**, and **homepage URL** — both
  URLs must be absolute `https://` URLs
- every **redirect URI** you will use, as absolute `https://` URLs
  (`http://` is accepted only on loopback hosts — `localhost`,
  `127.0.0.1`, `[::1]` — for local development)

Redirect URIs match **exactly, byte for byte** — `https://app.example/cb`
and `https://app.example/cb/` are different registrations. Query strings are
part of the URI; fragments are not allowed.

The admin hands back two values, shown once:

- `client_id` — public, identifies your application
- `client_secret` — authenticate at the token endpoint; keep it server-side

## Hosting providers

A **hosting provider** is a privileged kind of client (`kind =
hosting_provider`, set by an admin at creation — a standard client can
never claim it). A partner that runs snake servers for players. Hosting
clients carry three extra fields at creation:

- **Onboarding URL** — an absolute `https://` URL; the create-snake
  "Host with a partner" card sends players here.
- **Description** — one short line shown on the card.
- **Hosting domains** — the registered domains. A snake URL matches a
  client when its URL's host **equals** a registered domain or is a
  **subdomain** of one (`api.softmax.com` matches a `softmax.com`
  registration). The match is label-boundary: `softmax.com.evil.io` and
  `evilsoftmax.com` do NOT match `softmax.com`. Ports are ignored, hosts
  are lowercased, and IDN hosts are punycoded before matching.

Domains are **ASCII only** — international domains must be entered in
punycode (`xn--…`) form; anything non-ASCII is rejected with the punycode
form in the error. Trailing-dot forms are **not** normalized
(`softmax.com.` does not match a `softmax.com` registration).

Marking a snake hosted: `PUT /api/snakes/{id}/hosting` with the client's
token (empty body, `204 No Content`). The token must hold `snake.url.write`
for that snake, the client must be a hosting provider, and the snake's
current URL must sit on one of the client's registered domains — else
`400`. Another provider already hosting the snake, or the URL changing
mid-request, is a `409` (retry is safe; marking is idempotent for the
same client).

The **"Hosted on X" badge** follows the URL: any URL change through any
path (web edit, API, moderation) that moves the snake off the hosting
client's domains clears the mark in the same database update — the badge
can never disagree with the URL. The owner releases a snake by changing
its URL; there is no "stop hosting" button.

**Hosting fields are create-time only.** Changing a partner's domains,
onboarding URL, or description means disabling the client and creating a
new one — which kills every player's grant for it (disabled clients fail
token authentication) and leaves existing marks pointing at the disabled
client until each snake's URL changes. A small "edit hosting fields"
admin form is a named follow-up, deliberately not built yet.

Per-client game quotas are an **open question**: today the per-user
game-creation rate limit applies to everyone equally. A per-client quota
would plug into `game_creation_attempts` (add `oauth_client_id`, count per
client) — deliberately not built.

## The authorization request

Send the player's browser to:

```
GET {base}/oauth/authorize
```

| Parameter                | Required | Meaning                                                    |
|--------------------------|----------|------------------------------------------------------------|
| `client_id`              | yes      | your client id                                             |
| `redirect_uri`           | yes      | one of your registered URIs, exactly                        |
| `response_type`          | yes      | `code`                                                     |
| `scope`                  | yes      | space-separated permission names (below)                   |
| `state`                  | recommended | random string; echoed back on every redirect             |
| `code_challenge`         | yes      | `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`           |
| `code_challenge_method`  | yes      | `S256`                                                     |

The player must be logged in to Arena; if they aren't, they're sent through
GitHub login and returned to the same authorization request.

If anything other than the client id and redirect URI is wrong, Arena
redirects back to your `redirect_uri` with `error=` (and your `state`):

- `unsupported_response_type` — `response_type` isn't `code`
- `invalid_request` — missing/short PKCE challenge or method isn't `S256`
- `invalid_scope` — an unknown permission name, or `tokens.manage`/`admin`
  (those are personal-only forever)
- `access_denied` — the player denied the request

An **unknown client or an unregistered `redirect_uri` never redirects** —
Arena shows an error page instead, because there is no URI it can trust.

## The approval screen

The player sees your application's name and logo and a plain-words list of
every requested permission. They choose which of their snakes the app may
touch, and they can uncheck your email address (`email.read`) — that is the
only permission that can be withheld, and it is the only way the granted
scope can be narrower than what you requested.

On approval, the browser goes to your `redirect_uri`:

```
{redirect_uri}?code=…&state=…
```

The code is single-use and expires after **10 minutes**.

## PKCE

Generate a `code_verifier` — 43–128 characters of unreserved characters —
and derive the challenge:

```bash
VERIFIER="$(openssl rand -base64 48 | tr -d '=+/' | cut -c1-64)"
CHALLENGE="$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')"
```

Send `code_challenge=$CHALLENGE&code_challenge_method=S256` in the
authorize request, then prove you hold the verifier at the token exchange.

## The token exchange

The request body must be `application/x-www-form-urlencoded` (JSON bodies
are rejected with `invalid_request`).

```bash
curl -s -X POST {base}/oauth/token \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  -d "grant_type=authorization_code" \
  -d "code=$CODE" \
  -d "redirect_uri=$REDIRECT_URI" \
  -d "code_verifier=$VERIFIER" \
  -u "$CLIENT_ID:$CLIENT_SECRET"
```

Authenticate the client either in the body (`client_id` + `client_secret`
form fields) or with HTTP Basic auth (both halves form-urlencoded-encoded,
per RFC 6749 §2.3.1) — exactly one method per request. On success:

```json
{
  "access_token": "…64 hex chars…",
  "token_type": "Bearer",
  "scope": "profile.read snakes.read"
}
```

`scope` lists the permissions actually granted, which may be fewer than you
requested (a withheld `email.read`). Use the token as a normal Arena API
bearer token:

```bash
curl -s {base}/api/me -H "Authorization: Bearer $ACCESS_TOKEN"
```

Access tokens do not expire. They are revoked when the player revokes your
app or the grant's token.

> **Never redeem a code twice.** A replayed code fails — and it also
> **revokes the access token that code issued and the grant it created**
> (RFC 6749 §4.1.2). Treat a successful exchange as final; never blindly
> retry it.
>
> Codes are also invalidated by the player: revoking the app, editing its
> snake selection, deleting its token, or re-authorizing kills every
> outstanding code for your client. Don't sit on a code — exchange it as
> soon as the player approves.

### Errors

The token endpoint always answers JSON (`{"error": "…"}`):

- `invalid_client` (401) — unknown client, wrong secret, or a disabled
  client
- `unsupported_grant_type` (400) — anything but `authorization_code`
- `invalid_request` (400) — missing parameter, two client-auth methods, a
  body that isn't valid `application/x-www-form-urlencoded`, or a
  non-form content type
- `invalid_grant` (400) — unknown/expired/replayed code, wrong client,
  wrong `redirect_uri`, or a wrong `code_verifier`
- `temporarily_unavailable` (500) — Arena is temporarily unable to handle
  the request; retry later

## Permissions

Permissions are granted exactly as requested (except a withheld
`email.read`). Two of them are account-wide; the rest act only on the
snakes in the player's selection.

| Name              | What it lets the app do                                        |
|-------------------|----------------------------------------------------------------|
| `profile.read`    | See the player's public profile.                               |
| `email.read`      | See the player's email address. Optional — the player may withhold it. |
| `snakes.read`     | See the names and settings of **all** the player's battlesnakes (not their URLs). Account-wide: the snake selection does not limit it. |
| `snakes.create`   | Create battlesnakes for the player. Account-wide.              |
| `games.read`      | See the player's games. Account-wide.                          |
| `games.create`    | Start games using **any** of the player's battlesnakes. Account-wide. |
| `snake.url.read`  | See where a battlesnake runs (its URL), for the snakes in the selection. |
| `snake.url.write` | Change where a battlesnake runs (its URL), for the snakes in the selection. This lets the app take control of where the snake runs. |
| `snake.edit`      | Edit a battlesnake's name, visibility, and region, for the snakes in the selection. |
| `snake.delete`    | Delete battlesnakes **this app created** for the player. The app can never delete a snake the player made themselves. |
| `snake.compete`   | Enter and leave leaderboards with the player's battlesnakes, for the snakes in the selection. |

`tokens.manage` and `admin` cannot be requested by an OAuth client —
requesting either is an `invalid_scope` error.

## The snake-access model

On the approval screen the player picks one of:

- **All of my snakes** — the app's per-snake permissions cover every snake
  the player owns, now and in the future.
- **Only the snakes I pick** — an explicit list; snakes the app creates
  through a `selected` token join the list.

The selection gates the per-snake permissions above (`snake.url.read`,
`snake.url.write`, `snake.edit`, `snake.delete`, `snake.compete`). It does
**not** gate the account-wide permissions: with `snakes.read` the app sees
the names of all the player's snakes; with `games.create` it can start
games with any of them.

The player can change the selection later (or revoke the app entirely) on
their **Authorized apps** page (`/settings/authorized-apps`), linked from
their profile. Changing the selection takes effect immediately — the next
request with your token sees the new scope. Re-authorizing replaces the old
grant: your previous token stops working.

## A complete curl walkthrough

```bash
BASE="https://arena.example"          # your arena deployment
CLIENT_ID="…"                          # from the admin
CLIENT_SECRET="…"
REDIRECT_URI="https://app.example/cb"  # a registered URI

# 1. PKCE pair
VERIFIER="$(openssl rand -base64 48 | tr -d '=+/' | cut -c1-64)"
CHALLENGE="$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')"

# 2. Send the player's browser here (they log in and approve):
echo "$BASE/oauth/authorize\
?client_id=$CLIENT_ID\
&redirect_uri=$REDIRECT_URI\
&response_type=code\
&scope=profile.read%20snakes.read%20snake.url.read\
&state=xyz\
&code_challenge=$CHALLENGE\
&code_challenge_method=S256"

# 3. Your callback receives ?code=…&state=xyz (check the state!).

# 4. Exchange:
curl -s -X POST "$BASE/oauth/token" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$CODE" \
  --data-urlencode "redirect_uri=$REDIRECT_URI" \
  --data-urlencode "code_verifier=$VERIFIER" \
  -u "$CLIENT_ID:$CLIENT_SECRET"

# 5. Use it (this token has no games.create — a forbidden call is a 403):
curl -s "$BASE/api/me" -H "Authorization: Bearer $ACCESS_TOKEN"
curl -s "$BASE/api/snakes" -H "Authorization: Bearer $ACCESS_TOKEN"
curl -i -X POST "$BASE/api/games" -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H 'Content-Type: application/json' -d '{"snakes": []}'   # → 403
```
