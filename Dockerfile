FROM rust:1.71.0 as build
ENV PKG_CONFIG_ALLOW_CROSS=1

RUN USER=root cargo new docktail
WORKDIR /docktail
RUN touch ./src/lib.rs
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml

RUN cargo build --release
RUN rm src/*.rs
RUN find target -name "*docktail*" -print0 | xargs -0 rm -rf

COPY ./src ./src

RUN cargo build --release

FROM debian:bullseye
WORKDIR /docktail
COPY --from=build /docktail/target/release/docktail docktail
CMD ["./docktail"]
