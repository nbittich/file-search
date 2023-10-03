FROM rust:1.72.1 as builder

RUN apt update &&  apt upgrade -y && apt install -y libssl-dev build-essential cmake

WORKDIR /app

RUN cargo new file-search

WORKDIR /app/file-search

COPY ./Cargo.toml ./Cargo.lock ./

RUN cargo build --release 

RUN rm -rf ./src

COPY ./src/ ./src

RUN rm ./target/release/deps/file_search*

RUN cargo build --release 

FROM debian:bullseye-slim AS runtime
RUN apt  update && apt upgrade -y
RUN apt install -y ca-certificates 

# Set timezone
ENV TZ="Europe/Brussels"

ENV RUST_LOG=info

VOLUME /root/.local/share

COPY --from=builder  /app/file-search/target/release/file-search .

ENTRYPOINT [ "./file-search" ]
