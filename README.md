# chaz

Chaz is chaz.

This is a [Matrix](https://github.com/sigoden/aichat) bot that connects to [AIChat](https://github.com/sigoden/aichat) to provide access to "10+ AI platforms, including OpenAI, Gemini, Claude, Mistral, LocalAI, Ollama, VertexAI, Ernie, Qianwen..." all from within Matrix.

You do _NOT_ need to be running your own Matrix homeserver to use this.
It is a bot that should be usable with any homeserver, you'll just need to create an account for it.

You will need your own API keys or your own local AI already configured.

This is built using [headjack](https://github.com/arcuru/headjack), a Matrix bot framework developed alongside it.

Announcement Blog Post: [Chaz: An LLM <-> Matrix Chatbot](https://jackson.dev/post/chaz/)

## Getting Help

There is a public Matrix room available at [#chaz:jackson.dev](https://matrix.to/#/#chaz:jackson.dev)

## Usage

Chaz will automatically accept Room invites for any user in the `allow_list`.

When it's in a room, it will watch for commands that are prefixed by `!chaz`.
If it's a DM, it will respond to every message that it doesn't recognize as a command.
If it's in a larger room, it will only respond to messages that are sent to it using `!chaz`.

So in a larger room, send just `!chaz` and it will be sent all the recent messages in the room and asked for a response.
You can also send a request along with that, e.g. `!chaz explain that to me`, and it will receive your message and the context of the room and respond.

The commands that it recognizes are:

```markdown
!chaz help

Available commands:
!chaz print - Print the conversation
!chaz send <message> - Send a message without context
!chaz model <model> - Select the model to use
!chaz list - List available models
!chaz clear - Ignore all messages before this point
!chaz rename - Rename the room and set the topic based on the chat content
!chaz help - Show this message
```

## Install

`chaz` is only packaged on crates.io, but it's recommended that you run from git HEAD for now.

For [Nix](https://nixos.org/) users, this repo contains a Nix flake. See the [setup section](#nix) for details on configuring.

## Setup

First, setup an account on any Matrix server for the bot to use.

Create a config file for the bot with its login info.

**IMPORTANT**: Make sure that you setup your allow_list or the bot will not respond

The defaults are configured in [src/defaults.rs](src/defaults.rs)

```yaml
homeserver_url: https://matrix.org
username: "chaz"
password: "" # Optional, if not given it will ask for it on first run
allow_list: "" # Regex for allowed accounts.
message_limit: 0 # Set a per-account message limit. 0 = Unlimited.
room_size_limit: 0 # Set a room size limit to respond in. 0 = Unlimited
state_dir: "$XDG_STATE_HOME/chaz" # Optional, for setting the chaz state directory
aichat_config_dir: "$AICHAT_CONFIG_DIR" # Optional, for using a separate aichat config
chat_summary_model: "" # Optional, set a different model than the default to use for summarizing the chat
disable_media_context: false # Optional, set to true to disable sending media context to aichat
role: chaz # Optionally set a role, AKA system prompt. Set to `chaz` for the full chaz experience, or `cave-chaz` for even more chaz
roles: # Optional, define your own roles
  - name: chaz # This one is predefined
    description: Chaz is Chaz
    prompt: "Your name is Chaz, you are an AI assistant, and you refer to yourself in the third person."
    example: # Optionally define example messages.
      - user: User
        message: "Are you ready?"
      - user: Assistant
        message: "Chaz is ready."
  - name: bash
    description: Get a single shell command
    prompt: >
      Based on the following user description, generate a corresponding Bash shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Bash command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Bash command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
```

## Docker

There is a docker image available on [Docker Hub](https://hub.docker.com/r/arcuru/chaz).
Here's a Docker Compose example:

```yaml
services:
  chaz:
    image: arcuru/chaz:main # Set to your desired version
    restart: unless-stopped
    network_mode: host
    volumes:
      # Mount your config file to /config.yaml
      - ./config.yaml:/config.yaml
      # Mount your aichat config to /aichat, AND SET THAT LOCATION IN CHAZ'S CONFIG.YAML
      - aichat-state:/aichat
      - ./aichat.yaml:/aichat/config.yaml
      # Mount the volume into the same location specified in config.yaml
      - chaz-state:/state

volumes:
  # Persists the logged in session
  chaz-state:
  aichat-state:
```

Note that this requires 2 config files to be mounted into the container, one for chaz and one for aichat.
You'll also need to set the state/cache directories in your chaz config file.

The chaz config file should look something like this:

```yaml
homeserver_url: https://matrix.jackson.dev
username: "chaz"
password: ""
state_dir: "/state"
aichat_config_dir: "/aichat"
allow_list: "@.*:jackson.dev|@arcuru:matrix.org"
```

### Nix

Development is being done using a [Nix flake](https://nixos.wiki/wiki/Flakes).
The easiest way to install chaz is to use nix flakes.

```bash
❯ nix run github:arcuru/chaz
```

The flake contains an [overlay](https://nixos.wiki/wiki/Overlays) to make it easier to import into your own flake config.
To use, add it to your inputs:

```nix
    inputs.chaz.url = "github:arcuru/chaz";
```

And then add the overlay `inputs.chaz.overlays.default` to your pkgs.

The flake also contains a home-manager module for installing chaz as a service.
Import the module into your home-manager config and you can configure `chaz` all from within nix:

```nix
{inputs, ... }: {
  imports = [ inputs.chaz.homeManagerModules.default ];
  services.chaz = {
    enable = true;
    settings = {
        homeserver_url = "https://matrix.jackson.dev";
        username = "chaz";
        password = "hunter2";
        allow_list = "@me:matrix.org|@myfriend:matrix.org";
    };
  };
}
```

## Running

To run it, simply:

1. Install _chaz_ and setup its config.
2. Install [AIChat](https://github.com/sigoden/aichat).
3. Configure [AIChat](https://github.com/sigoden/aichat) with the models and defaults that you want.
4. Create a config file for _chaz_ with login details.
5. Run the bot and specify it's config file location `chaz --config config.yaml`.

The bot will not respond to older messages sent while it wasn't running to prevent overwhelming the backend.
