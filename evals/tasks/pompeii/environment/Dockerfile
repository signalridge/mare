FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates file \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir jsonschema==4.25.1
COPY pompeii-rp040.zip /tmp/pompeii-rp040.zip
RUN mkdir -p /app && python3 -m zipfile -e /tmp/pompeii-rp040.zip /app && rm /tmp/pompeii-rp040.zip
WORKDIR /app
