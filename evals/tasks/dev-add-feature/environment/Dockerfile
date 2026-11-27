FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates file git \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir pytest==8.4.2
WORKDIR /app
COPY repo/ /app/
RUN git init -q && git add -A && git -c user.name=dev -c user.email=dev@example.com commit -qm "shopcart 0.3.0"
