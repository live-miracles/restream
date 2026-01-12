# Restreamer

A streaming server which takes inputs and replicates them to multiple outputs.

## Design

<img src="https://github.com/user-attachments/assets/499f3309-eed3-49b3-8eaf-6d3a365751de" align="center" width="500" >

### Basic features:

- User can create a stream and give it a name (i.e. English or Spanish)
- User can select a stream key for the stream or generate a new one.
- For each stream user can add multiple outputs.
- User can select encoding for each output. Some options are: source, rotate.
- Once stream receives feed you can turn On/Off the outputs. This can be done dynamically without disturbing other outputs.
- There should be two input RTMPs for each stream: main and backup. User should be able to manually select which one should be used for outputs.

### Other features:

- User can upload a video to the server.
- User can use the video as a source feed for any of the streams.
- User can start stream recording.

## Contribute

After cloning, run:

```sh
git config core.hooksPath .githooks
```

## Links

1. MediaMTX: [github.com/bluenviron/mediamtx](https://github.com/bluenviron/mediamtx)
2. Nginx MLS: [github.com/live-miracles/MLS](https://github.com/live-miracles/MLS)
3. Nginx MLS demo: [youtu.be/yzXuirkmexo](https://youtu.be/yzXuirkmexo)
4. Go-MLS: [github.com/krsna1729/go-mls](https://github.com/krsna1729/go-mls)
5. Go-MLS demo: [www.youtube.com/watch?v=x2x3uSCAX4M](https://www.youtube.com/watch?v=x2x3uSCAX4M)
6. Daisy UI: [daisyui.com/](https://daisyui.com/)
