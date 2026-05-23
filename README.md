# hymt

CLI for translating text and files with Hy-MT2 through an OpenAI-compatible chat completions endpoint.

## Usage

```bash
hymt "Hello world" -t zh
hymt -f input.txt -t en -o output.txt
cat article.txt | hymt -t ja
```

Configuration is stored at `~/.config/hymt/config.toml` and is created with defaults on first use.
