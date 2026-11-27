FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates file \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /srv/reports
WORKDIR /app
COPY files/ /app/
# stands in for the user who answers approval prompts (headless has nobody): see the plugin
COPY plugins/ /etc/mare-eval/plugins/
