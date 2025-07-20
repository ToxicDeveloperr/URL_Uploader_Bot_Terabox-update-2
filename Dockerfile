# Build stage
FROM rust:1.75-slim as builder

# Create a new empty shell project
WORKDIR /usr/src/app
COPY . .

# Build with release profile
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install SSL certificates (needed for HTTPS requests)
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/url-uploader .

# Create a directory for persistent data
RUN mkdir -p /app/data

# Environment variables will be provided at runtime
ENV API_ID=
ENV API_HASH=
ENV BOT_TOKEN=

# Run the binary
CMD ["./url-uploader"]
