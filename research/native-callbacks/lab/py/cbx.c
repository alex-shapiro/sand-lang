/* CPython C-extension callback experiments. */
#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* E1: native calls a script callback n times. */
static PyObject *repeat(PyObject *self, PyObject *args) {
  PyObject *f; long long n, acc = 0;
  if (!PyArg_ParseTuple(args, "OL", &f, &n)) return NULL;
  for (long long i = 0; i < n; i++) {
    PyObject *a = PyLong_FromLongLong(i);
    PyObject *r = PyObject_CallOneArg(f, a);
    Py_DECREF(a);
    if (!r) return NULL;
    acc += PyLong_AsLongLong(r); Py_DECREF(r);
  }
  return PyLong_FromLongLong(acc);
}
/* E2: native<->script mutual recursion. */
static PyObject *bounce(PyObject *self, PyObject *args) {
  PyObject *f, *d;
  if (!PyArg_ParseTuple(args, "OO", &f, &d)) return NULL;
  return PyObject_CallOneArg(f, d);
}
/* E3: call with no arguments. */
static PyObject *call(PyObject *self, PyObject *f) { return PyObject_CallNoArgs(f); }
/* E4: errors propagate as NULL, so native cleanup runs. */
static int acquired = 0, cleaned = 0;
static PyObject *with_resource(PyObject *self, PyObject *f) {
  acquired++;
  PyObject *r = PyObject_CallNoArgs(f);
  cleaned++;
  return r;
}
static PyObject *counts(PyObject *self, PyObject *unused) { return Py_BuildValue("ii", acquired, cleaned); }

static PyMethodDef methods[] = {
  {"repeat", repeat, METH_VARARGS, NULL}, {"bounce", bounce, METH_VARARGS, NULL},
  {"call", call, METH_O, NULL}, {"with_resource", with_resource, METH_O, NULL},
  {"counts", counts, METH_NOARGS, NULL}, {NULL, NULL, 0, NULL}};
static struct PyModuleDef mod = {PyModuleDef_HEAD_INIT, "cbx", NULL, -1, methods};
PyMODINIT_FUNC PyInit_cbx(void) { return PyModule_Create(&mod); }
