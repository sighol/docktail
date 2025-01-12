run-alpine:
	docker build -f Dockerfile.alpine . -t docktail:alpine-3.15
	docker run --rm -it \
		-e LOKI_URL="http:loki:3100/loki/api/v1/push" \
		--network docktail_default \
		--volume "/var/run/docker.sock:/var/run/docker.sock" \
		docktail:alpine-3.15 sh
