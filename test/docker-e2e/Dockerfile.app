FROM node:22-slim

WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

ENV NODE_ENV=production \
    PORT=3030

EXPOSE 3030
CMD ["node", "dist/index.js"]
