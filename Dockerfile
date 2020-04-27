#FROM golang:1.13-alpine3.10 AS builder
FROM golang:alpine3.10 AS builder

RUN apk add --no-cache git ca-certificates

WORKDIR /app

COPY . .

# The image should be built with
# --build-arg ST_VERSION=`git describe --tags --always`
ARG ST_VERSION
RUN if [ ! -z "$ST_VERSION" ]; then sed -i "s/UNKNOWN_RELEASE/${ST_VERSION}/g" smtp_to_telegram.go; fi

RUN CGO_ENABLED=0 GOOS=linux go build \
        -ldflags "-s -w" \
        -a -o smtp_to_telegram





FROM alpine
#FROM alpine:3.10

RUN apk add --no-cache ca-certificates

COPY --from=builder /app/smtp_to_telegram /smtp_to_telegram

USER daemon

ENV ST_SMTP_LISTEN "0.0.0.0:2525"
EXPOSE 2525

ENTRYPOINT ["/smtp_to_telegram"]
