# Steam Integration

`voice-orders run` can launch an application as a child process and exit when it exits. That one behaviour is what
makes it a drop-in Steam launch option: Steam thinks it is running your game, and voice-orders lives and dies with it.

## The launch option

Right-click the game in your Steam library → **Properties** → **General** → **Launch Options**, and set:

```
voice-orders run /home/you/profiles/drg.yaml -- %command%
```

Steam replaces `%command%` with everything it would otherwise have run — the game's executable, its arguments, and any
wrapper such as Proton. Everything after the `--` separator is treated as the application to launch, so the
substitution lands intact no matter how many arguments Steam adds.

::: tip
Use an absolute path for the profile. Steam does not run launch options from the directory you think it does, and a
relative path is the most common reason for a "we could not read the profile" error here.
:::

If `voice-orders` is not on the `PATH` your Steam session sees, give the absolute path to it too:

```
/home/you/.local/bin/voice-orders run /home/you/profiles/drg.yaml -- %command%
```

## What actually happens

1. voice-orders loads and validates your profile, and fails immediately (before the game starts) if anything is wrong
   with it.
2. It creates its uinput virtual keyboard, so a permissions problem also surfaces before the game starts.
3. It loads the model, opens your microphone, and starts listening.
4. It spawns the game with stdio inherited, so the game's own output still reaches Steam's console as usual.
5. When the game exits, voice-orders shuts down and **exits with the game's exit code**.

That last point is the contract Steam cares about: as far as the client is concerned the game ran and finished normally,
so playtime tracking, the "Stop" button, and the in-library running indicator all behave as they always did.

Shutdown is ordered so that nothing is left in a strange state: the pipeline is cancelled, the executor **releases every
key it is still holding down**, and only then does the virtual keyboard go away. A hold-style macro can never leave `W`
pressed after your game closes.

Signals are handled the way you would expect. `SIGINT` (Ctrl-C in a terminal) reaches the child too, because it shares
the process group. `SIGTERM` — which is what Steam's "Stop" button sends — is forwarded to the child, followed by a
short grace period before voice-orders exits regardless.

## Per-game profiles

Each game gets its own profile file, and each launch option names the profile it wants:

```
# Deep Rock Galactic
voice-orders run /home/you/profiles/drg.yaml -- %command%

# Elite Dangerous
voice-orders run /home/you/profiles/elite.yaml -- %command%
```

There is no runtime profile switching: one `voice-orders run` listens for exactly one profile's commands for its whole
life. That is deliberate — the recognition grammar is compiled from the profile at startup, and a grammar containing
only the phrases *this* game can act on is what makes recognition as accurate as it is.

You can also point a launch option straight at a profile published online, which is a nice way to use a profile someone
else maintains:

```
voice-orders run https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db -- %command%
```

Profiles are only fetched over HTTPS and are never cached — a `run` at game-launch time fails loudly rather than
silently starting with a stale profile. If your network is unreliable, download the profile to a local file and point at
that instead. See [loading profiles from a URL](../profiles/README.md#loading-a-profile-from-a-url) for the details.

## Proton and native games

The wrapper does not care which it is. Steam's `%command%` already contains whatever Proton invocation is required, and
voice-orders simply spawns it. The keystrokes go through a kernel-level virtual keyboard, so they arrive at a Proton
game exactly as they arrive at a native one.

## Troubleshooting

### The game launches but nothing is recognized

If your profile has a [`hotkey:`](../profiles/README.md#hotkey) block in `toggle` or `push-to-talk` mode, voice-orders
starts **muted** — press the hotkey once. (`push-to-mute` starts listening.) To listen from the moment the game starts,
leave the `hotkey:` block out entirely.

If that is not it, quit the game and run [`voice-orders test`](../profiles/README.md#rehearsing-a-profile) on the same
profile: it prints every utterance it hears, every command it matches, and every change of listening state, which
settles whether the problem is the microphone, the hotkey, or the phrases.

### The game does not launch at all

Run the same command in a terminal, without Steam, to see the error:

```sh
voice-orders run /home/you/profiles/drg.yaml -- /path/to/game
```

Profile, permission and model errors are all reported with an explanation and advice; Steam's console tends to swallow
them. `voice-orders doctor /home/you/profiles/drg.yaml` will diagnose the permission and model side without launching
anything, and `voice-orders test /home/you/profiles/drg.yaml` lets you rehearse the profile itself.

### The keys arrive but the game ignores them

Some games filter input devices, and a few anti-cheat systems are suspicious of uinput. voice-orders mitigates the first
by registering the full key capability set under a keyboard-like device name, which is enough for SDL to treat it as a
real keyboard. The second is a known limitation rather than something this tool can solve — check your game's
anti-cheat policy before relying on voice macros in multiplayer.
