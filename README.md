Want to play the 2002 movie "Rumble" (imdb tt0347909) starring Vesa-Matti Loiri, Tommi Korpela et. al on your Disobey badge but can't find the right firmware?

Well, now it's possible with rumble-rs. Me and my dear friend Claudea Opus developed a firmware to play an mjpeg video stream over Wi-Fi with esp-rs, embassy and Espressif's new ESP_NEW_JPEG decoder. Runs at full resolution (320x170) and full FPS (at least for the 24 fps movie I was testing it with).

If you want to try it, clone the repo, install rustup, espup, then install the rust toochain for esp32 with espup install, source ~/export-esp.sh to activate the toolchain, run cargo run --release to build and deploy the firmware. Then setup a wifi hotspot with the ssid ylikellotus and password alakerta, get yourself the IP 172.20.10.8 (these are hardcoded, can be changed), and start streaming your favorite video with the following ffmpeg command:

```sh
ffmpeg -ss 00:20:20 -re -i vid.mkv -vf "scale=320:176:force_original_aspect_ratio=increase,crop=320:176" -c:v mjpeg -q:v 5 -an -f mjpeg tcp://0.0.0.0:3000?listen
```

Alternatively, check out the demo video https://drive.google.com/file/d/1_fcG7YBJdDS2tkKw663mwGIDgHBqjjhq/view?usp=sharing

or the second demo video at https://drive.google.com/file/d/1lTY2BYsVBhU1uwVIOgLoQeFeKim9HAWd/view?usp=drivesdk
