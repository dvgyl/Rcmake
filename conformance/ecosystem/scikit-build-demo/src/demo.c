// SPDX-License-Identifier: MIT OR Apache-2.0
#include <Python.h>

static struct PyModuleDef module = {
    PyModuleDef_HEAD_INIT, "rcmake_scikit_probe", 0, -1, 0, 0, 0, 0, 0
};

PyMODINIT_FUNC PyInit_rcmake_scikit_probe(void)
{
    return PyModule_Create(&module);
}
