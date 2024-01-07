# Gstreamer KLV test

What it should do:

* grab frames from the camera
* add some KLV metadata for each fram
* mux video and KLV to a stream
* demux video and KLV (muxing and demuxing simulates that there is some transport step inbetween)
* show video stream and print or better yet overlay KLV data to the frame
* frames and KLV data must be in sync, we would like to know for which frame this particular KLV metadata was generated

## Run

```bash
RUST_LOG=info cargo run --release
```

## Issues

* [FIXED]: From [log.txt](log.txt) can be seen that every 5th frame takes 233 ms instead of 33 ms as it should. What is causing it?
