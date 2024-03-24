# headjack

Jack some (AI) heads into Matrix.

This is a [Matrix](https://github.com/sigoden/aichat) bot that connects to [AIChat](https://github.com/sigoden/aichat) to provide access to "10+ AI platforms, including OpenAI, Gemini, Claude, Mistral, LocalAI, Ollama, VertexAI, Ernie, Qianwen..." all from within Matrix.

You do _NOT_ need to be running your own Matrix homeserver to use this.
It is a bot that should be usable with any homeserver, you'll just need to create an account for it.

You will need your own API keys or your own local AI already configured.

## Install

This is not packaged anywhere, so you'll have to clone and install it yourself.

## Setup

First, setup an account on any Matrix server for the bot to use.

Create a config file for the bot with its login info.

**IMPORTANT**: Make sure that you setup your allow_list or the bot will not respond

```yaml
homeserver_url: https://matrix.org
username: "headjack"
password: "" # Optional, if not given it will ask for it on first run
allow_list: "" # Regex for allowed accounts.
aichat_config_dir: "$AICHAT_CONFIG_DIR" # Optional, for using a separate aichat config
chat_summary_model: "" # Optional, set a different model than the default to use for summarizing the chat
```

## Running

To run it, simply:

1. Install _headjack_ and setup its config.
2. Install [AIChat](https://github.com/sigoden/aichat).
3. Configure [AIChat](https://github.com/sigoden/aichat) with the models and defaults that you want.
4. Create a config file for _headjack_ with login details.
5. Run the bot and specify it's config file location `headjack --config config.yaml`.

The bot will not respond to older messages sent while it wasn't running to prevent overwhelming the backend.
