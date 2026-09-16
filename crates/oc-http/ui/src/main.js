import { createApp } from 'vue'
import App from './App.vue'
import { handleAuthFailure } from './lib/api.js'

const app = createApp(App)

// Central error sink: an AuthError (401) anywhere in a render/lifecycle/event
// handler means the login cookie is missing or expired — route it to the same
// exit every other auth-failure path uses.
app.config.errorHandler = (err, _instance, info) => {
  if (handleAuthFailure(err)) return
  console.error(`[Vue error] ${info}:`, err)
}

app.mount('#app')
