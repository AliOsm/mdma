// The shipped scene bundle and stylesheet on a stub fleet: `just web-preview`.
const web = new URL('./', import.meta.url).pathname
const assets = new URL('../src/bin/mdma-web/assets/', import.meta.url).pathname
const routes = {
  '/': `${web}preview.html`,
  '/assets/scene.js': `${assets}scene.js`,
  '/assets/style.css': `${assets}style.css`,
  '/preview.js': `${web}src/preview.js`,
  '/stub.js': `${web}src/stub.js`,
}

const server = Bun.serve({
  hostname: '127.0.0.1',
  port: Number(process.env.PORT ?? 3000),
  fetch(request) {
    const path = routes[new URL(request.url).pathname]
    return path ? new Response(Bun.file(path)) : new Response('not found', { status: 404 })
  },
})
console.log(`mdma federation preview: ${server.url}?peers=100`)
