# battlesnake CLI

The `battlesnake` CLI runs complete Battlesnake games locally, on your machine, against your snake URLs — no arena account needed. Point `battlesnake play` at one or more running snakes and it drives the game turn by turn, printing the board and calling your snakes' endpoints, exactly the way arena's engine does.

## Install — prebuilt binary (recommended)

Download an archive from [GitHub Releases](https://github.com/BattlesnakeOfficial/arena/releases). One archive per platform:

| Platform | Archive |
|---|---|
| Linux (x86_64) | `battlesnake_<version>_Linux_x86_64.tar.gz` |
| Linux (arm64) | `battlesnake_<version>_Linux_arm64.tar.gz` |
| Linux (i386) | `battlesnake_<version>_Linux_i386.tar.gz` |
| macOS (arm64) | `battlesnake_<version>_Darwin_arm64.tar.gz` |
| macOS (x86_64) | `battlesnake_<version>_Darwin_x86_64.tar.gz` |
| Windows (x86_64) | `battlesnake_<version>_Windows_x86_64.zip` |
| Windows (arm64) | `battlesnake_<version>_Windows_arm64.zip` |
| Windows (i386) | `battlesnake_<version>_Windows_i386.zip` |

Each archive contains the `battlesnake` binary (`battlesnake.exe` on Windows), the repository's `LICENSE`, and `ATTRIBUTION.md`. The Linux binaries are statically linked, so one archive runs on any Linux distribution.

macOS/Linux:

```sh
tar xzf battlesnake_<version>_Linux_x86_64.tar.gz
chmod +x battlesnake
./battlesnake --version
```

Windows: unzip the archive, then run `battlesnake.exe --version`.

To verify a download, also grab `checksums.txt` from the same release and check just the archive you use:

```sh
sha256sum -c checksums.txt --ignore-missing
```

## Install — cargo

```sh
cargo install --locked --git https://github.com/BattlesnakeOfficial/arena battlesnake
```

This needs only a Rust toolchain — no Postgres, no other setup. (Don't use bare `cargo install battlesnake`: that name belongs to an unrelated crate on crates.io.)

Without `--tag`, cargo builds from the latest commit on the default branch (`main`). To pin a release:

```sh
cargo install --locked --git https://github.com/BattlesnakeOfficial/arena --tag cli-v<version> battlesnake
```

## Your first local game

First, get a snake running on localhost. For a quick test, this minimal snake (Python 3 standard library only) always moves up:

```sh
curl -o minimal_snake.py https://raw.githubusercontent.com/BattlesnakeOfficial/arena/main/cli/examples/minimal_snake.py
python3 minimal_snake.py
```

Any real snake works too — see the [starter snakes](https://github.com/BattlesnakeOfficial?q=starter-snake) for Go, Rust, Python, JavaScript and TypeScript.

Then, in another terminal:

```sh
battlesnake play -W 11 -H 11 --name "My Snake" --url http://127.0.0.1:8085 -g solo -v
```

The CLI drives the game against your snake and prints the outcome.

Add `--browser` to watch the game live on [board.battlesnake.com](https://board.battlesnake.com) instead of the terminal.

## What this CLI runs today

- Game types (`-g`): `standard`, `solo`, `royale`, `constrictor`, `snail_mode`
- Board sizes: 7x7, 11x11, 19x19
- Snakes: 1 to 8
- Maps (`-m`): `standard`, `royale`, `empty`, `snail_mode`

Not ported yet: wrapped rulesets, board sizes other than the three above, more than 8 snakes, and the remaining 21 maps from the Go CLI. Until those land, the Go CLI ([`github.com/BattlesnakeOfficial/rules`](https://github.com/BattlesnakeOfficial/rules)) still runs them.

### Go CLI compatibility

Flag names and short forms match the Go CLI, so existing commands for the five game types above run unchanged. A few things behave differently on purpose, so local games match arena:

- A seed (`-r`) taken from the Go CLI will not reproduce the same game here: the engines use different RNGs.
- `-g royale` always has hazards (the Go CLI needed `-m royale` for them).
- `-g constrictor` never spawns food.
- An unknown `-g` value is an error that lists the valid game types — the Go CLI silently ran `standard`.
- Board sizes other than 7x7/11x11/19x19, maps outside the four above, and 9+ snakes exit with an "isn't available yet" error naming what is valid.

## More

- `battlesnake --help` — every subcommand
- `battlesnake play --help` — the full `play` flag surface
