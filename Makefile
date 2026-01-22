.PHONY: *

all:
	make start-ui

pretty:
	npx prettier --write .

css:
	npx @tailwindcss/cli -i ./ui/input.css -o ./ui/output.css

start-mediamtx:
	docker run --rm -it -p 8554:8554 -p 1935:1935 -p 8888:8888 -v ./mediamtx.yml:/mediamtx.yml bluenviron/mediamtx

start-ui:
	npm start

start-input:
	ffmpeg -re -stream_loop -1 -i ./test/colorbar-timer.mp4 -c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency -c:a aac -b:a 128k -ac 2 -f flv rtmp://localhost:1935/mystream
