# chaz

Chaz is chaz.

This is a [Matrix](https://matrix.org) bot that connects to multiple LLM providers to allow for chatting with any of the LLM models. It is compatible with any model using the OpenAI API.

In addition, it is also possible to use [AIChat](https://github.com/sigoden/aichat) as a provider, so any models available through AIChat can be accessed as well.

You will need your own API keys or your own local AI already configured.

Announcement Blog Post: [Chaz: An LLM <-> Matrix Chatbot](https://jackson.dev/post/chaz/)

## Getting Help

There is a public Matrix room available at [#chaz:jackson.dev](https://matrix.to/#/#chaz:jackson.dev)

## Getting Started

If you have your own API keys, and you trust me not to abuse them, you can get started quickly with [@chaz:jackson.dev](https://matrix.to/#/@chaz:jackson.dev).
Simply add that Matrix user to your room, configure it with your own API keys to an OpenAI API compatible backend, and you're good to go.

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
!chaz backend <name> <api_base> <api_key> - Manually enter an OpenAI Compatible Backend
!chaz role [<role>] [<prompt>] - Get the role info, set the role, or define a new role
!chaz list - List available models
!chaz clear - Ignore all messages before this point
!chaz rename - Rename the room and set the topic based on the chat content
!chaz help - Show this message
```

### Setting Roles

The `!chaz role` command takes 0, 1, or many arguments.

- Use `!chaz role` to show the current role and list all available roles.
- Use `!chaz role <name>` to set an existing role as the default.
- Use `!chaz role <name> <prompt>` to create a new role with the given prompt.

## Install

`chaz` is only packaged on crates.io, but it's recommended that you run from git HEAD for now.

For [Nix](https://nixos.org/) users, this repo contains a Nix flake. See the [setup section](#nix) for details on configuring.

### Docker

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
#message_limit: 0 # Set a per-account message limit, it will not allow more than this many messages per account.
#room_size_limit: 0 # Set a room size limit. It will refuse join if the room is too large.
state_dir: "$XDG_STATE_HOME/chaz" # Optional, for setting the chaz state directory
aichat_config_dir: "$AICHAT_CONFIG_DIR" # Optional, for using a separate aichat config
chat_summary_model: "" # Optional, set a different model than the default to use for summarizing the chat
disable_media_context: false # Optional, set to true to disable sending media context to aichat
role: chaz # Optionally set a role, AKA system prompt. Set to `chaz` for the full chaz experience, or `cave-chaz` for even more chaz
# Define backends. If more than 1 is defined, model names will be prefixed by the backends name.
# If none are defined, Chaz will look for Aichat
backends:
  - name: openai # Name of the backend, models will be shown with this as a prefix, e.g. openai:gpt-4
    type: openaicompatible
    api_key:
    api_base: https://api.openai.com/v1
    models: # Listing models here is not necessary, but does make Chaz aware of them. You can still switch to a model not listed here through '!chaz model ....'
      - name: gpt-4o
      - name: gpt-4o-mini
  - name: tog # Name can be anything. Model names will be "tog:<model>"
    type: openaicompatible
    api_key:
    api_base: https://api.together.xyz/v1
  - name: aic
    type: aichat
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

## Running

To run it, simply:

1. Install _chaz_ and setup its config.
2. Install [AIChat](https://github.com/sigoden/aichat).
3. Configure [AIChat](https://github.com/sigoden/aichat) with the models and defaults that you want.
4. Create a config file for _chaz_ with login details.
5. Run the bot and specify it's config file location `chaz --config config.yaml`.

The bot will not respond to older messages sent while it wasn't running to prevent overwhelming the backend.

## Nix

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

## Repository

**Mirrored on [GitHub](https://github.com/arcuru/chaz) and [Codeberg](https://codeberg.org/arcuru/chaz). GitHub is the official repo, but use either repo to contribute. Issues can't be synced so there may be some duplicates.**
