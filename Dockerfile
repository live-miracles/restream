FROM node:22-alpine

RUN apk add --no-cache ffmpeg

WORKDIR /app

COPY package.json package-lock.json ./
RUN npm ci --omit=dev

COPY src ./src
COPY public ./public

EXPOSE 3030

ENV NODE_ENV=production
ENV PORT=3030

CMD ["npm", "start"]