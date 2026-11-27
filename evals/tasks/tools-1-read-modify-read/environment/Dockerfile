FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates file \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY files/ /app/
