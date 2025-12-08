FROM rust:1-alpine AS build

WORKDIR /src

COPY Cargo.* ./

RUN mkdir src && touch src/lib.rs && cargo build --release

COPY .sqlx .sqlx

COPY src src

RUN touch src/lib.rs && cargo build --release

FROM alpine

WORKDIR /app

COPY --from=build /src/target/release/yaf2m .

ENTRYPOINT [ "./yaf2m" ]
