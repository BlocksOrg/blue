FROM node:22-bookworm-slim

WORKDIR /docs

ENV PUPPETEER_SKIP_DOWNLOAD=true

COPY package.json package-lock.json ./
RUN npm ci

COPY . .

EXPOSE 3000

CMD ["sh", "-c", "rm -f /root/.mintlify/preview-locks/3000.lock && exec npm run dev -- --port 3000 --no-open"]
