run-alpine:
	docker build -f Dockerfile.alpine . -t docktail:alpine-3.15
	docker run --rm -it \
		-e LOKI_URL="http:loki:3100/loki/api/v1/push" \
		--network docktail_default \
		--volume "/var/run/docker.sock:/var/run/docker.sock" \
		docktail:alpine-3.15 sh

upload-configs:
	rsync config_homeserver.json home-server:docktail/config.json
	rsync config_sighol3.json sighol3:docktail/config.json
	ssh home-server -- bash -c "cd docktail && docker compose restart docktail"
	ssh sighol3 -- bash -c "cd docktail && docker compose restart docktail"

upload-home-server:
	rsync config_homeserver.json home-server:docktail/config.json
	dep deploy -s home-server -r registry.home.sighol.com
	dep restart -s home-server -r registry.home.sighol.com
