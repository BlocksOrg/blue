FROM node:22-alpine AS build
ARG BLUE_DEPLOYMENT_VERSION=development
ENV BLUE_DEPLOYMENT_VERSION=$BLUE_DEPLOYMENT_VERSION
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM node:22-alpine
ARG BLUE_DEPLOYMENT_VERSION=development
WORKDIR /app
ENV NODE_ENV=production
ENV BLUE_DEPLOYMENT_VERSION=$BLUE_DEPLOYMENT_VERSION
COPY --from=build /app/.next/standalone ./
COPY --from=build /app/.next/static ./.next/static
COPY --from=build /app/public ./public
COPY --from=build /app/dashboard-start.mjs ./dashboard-start.mjs
COPY --from=build /app/lib/runtime-config.mjs ./lib/runtime-config.mjs
RUN rm -rf /usr/local/lib/node_modules/npm /usr/local/bin/npm /usr/local/bin/npx \
    && chown -R node:node /app
USER node
EXPOSE 3000
CMD ["node", "dashboard-start.mjs"]
