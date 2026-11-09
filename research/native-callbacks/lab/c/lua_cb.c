/* Lua C-API callback experiments. Build against lua-5.4.8 or lua-5.5.0. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "lua.h"
#include "lauxlib.h"
#include "lualib.h"

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec * 1e-9; }

/* E1: native calls a script callback n times: native.repeat(f, n) */
static int c_repeat(lua_State *L) {
  lua_Integer n = luaL_checkinteger(L, 2), acc = 0;
  for (lua_Integer i = 0; i < n; i++) {
    lua_pushvalue(L, 1); lua_pushinteger(L, i);
    lua_call(L, 1, 1);
    acc += lua_tointeger(L, -1); lua_pop(L, 1);
  }
  lua_pushinteger(L, acc); return 1;
}
/* E2: native that calls f(depth): mutual recursion native<->script */
static int c_bounce(lua_State *L) {
  lua_pushvalue(L, 1); lua_pushvalue(L, 2);
  lua_call(L, 1, 1); return 1;
}
/* E3a: plain lua_call: a yield inside the callback cannot cross this frame */
static int c_call_plain(lua_State *L) { lua_pushvalue(L, 1); lua_call(L, 0, 1); return 1; }
/* E3b: lua_callk with a continuation: the yield can cross this frame */
static int k_after(lua_State *L, int status, lua_KContext ctx) { (void)status; (void)ctx; lua_pushstring(L, "continuation ran"); return 2; }
static int c_call_k(lua_State *L) { lua_pushvalue(L, 1); lua_callk(L, 0, 1, 0, k_after); return k_after(L, LUA_OK, 0); }
/* E4: cleanup in the native is skipped when the callback errors (longjmp) */
static int cleanups = 0, acquired = 0;
static int c_with_resource(lua_State *L) {
  acquired++;                 /* "acquire" a host resource */
  lua_pushvalue(L, 1); lua_call(L, 0, 0);
  cleanups++;                 /* "release" it: skipped by longjmp on error */
  return 0;
}
static int c_counts(lua_State *L) { lua_pushinteger(L, acquired); lua_pushinteger(L, cleanups); return 2; }
/* E4: instruction-count hook: aborts runaway callbacks */
static long budget;
static void hook(lua_State *L, lua_Debug *ar) { (void)ar; budget -= 1000; if (budget < 0) luaL_error(L, "fuel exhausted"); }
static int c_set_budget(lua_State *L) { budget = luaL_checkinteger(L, 1); lua_sethook(L, hook, LUA_MASKCOUNT, 1000); return 0; }
/* E5: memory limit allocator */
static size_t used = 0, limit = (size_t)-1;
static void *alloc(void *ud, void *p, size_t osize, size_t nsize) {
  (void)ud; if (p == NULL) osize = 0;
  if (nsize == 0) { used -= osize; free(p); return NULL; }
  if (used - osize + nsize > limit) return NULL;
  void *q = realloc(p, nsize); if (q) used = used - osize + nsize; return q;
}
static int c_set_limit(lua_State *L) { limit = used + (size_t)luaL_checkinteger(L, 1); return 0; }
static int c_clear_limit(lua_State *L) { (void)L; limit = (size_t)-1; return 0; }
/* E7: keep a callback in the registry and call it later from the host */
static int handler_ref = LUA_NOREF;
static int c_on_event(lua_State *L) { lua_pushvalue(L, 1); handler_ref = luaL_ref(L, LUA_REGISTRYINDEX); return 0; }
static int c_now(lua_State *L) { lua_pushnumber(L, now()); return 1; }

static const luaL_Reg fns[] = {
  {"repeat_", c_repeat}, {"bounce", c_bounce}, {"call_plain", c_call_plain}, {"call_k", c_call_k},
  {"with_resource", c_with_resource}, {"counts", c_counts}, {"set_budget", c_set_budget},
  {"set_limit", c_set_limit}, {"clear_limit", c_clear_limit}, {"on_event", c_on_event}, {"now", c_now}, {NULL, NULL}};

int main(int argc, char **argv) {
  lua_State *L = lua_newstate(alloc, NULL
#if LUA_VERSION_NUM >= 505
    , 0
#endif
  );
  luaL_openlibs(L);
  luaL_newlib(L, fns); lua_setglobal(L, "native");
  lua_pushstring(L, LUA_RELEASE); lua_setglobal(L, "RELEASE");
  if (luaL_dofile(L, argv[1])) { fprintf(stderr, "error: %s\n", lua_tostring(L, -1)); return 1; }
  /* E7: host dispatches stored event handler later */
  if (handler_ref != LUA_NOREF) {
    lua_rawgeti(L, LUA_REGISTRYINDEX, handler_ref); lua_pushstring(L, "tap");
    if (lua_pcall(L, 1, 0, 0)) printf("E7 handler error: %s\n", lua_tostring(L, -1));
  }
  lua_close(L); (void)argc; return 0;
}
