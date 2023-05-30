# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under both the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree and the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree.

#
# Put everything inside an __invoke_main() function.
# This way anything we define won't pollute globals(), since runpy
# will propagate our globals() as to the user's main module.
# pyre-fixme[3]: Return type must be annotated.
def __invoke_main():
    import os
    import runpy
    import sys

    module = os.getenv("FB_LPAR_MAIN_MODULE")

    if decorate_main_module := os.environ.pop("PAR_MAIN_OVERRIDE", None):
        # Pass the original main module as environment variable for the process.
        # Allowing the decorating module to pick it up.
        # pyre-fixme[6]: For 2nd argument expected `str` but got `Optional[str]`.
        os.environ["PAR_MAIN_ORIGINAL"] = module
        module = decorate_main_module

    # pyre-fixme[6]: For 2nd argument expected `str` but got `Optional[str]`.
    sys.argv[0] = os.getenv("FB_LPAR_INVOKED_NAME")
    del sys.path[0]

    # Read `PYTHONDEBUGWITHPDB` before we cleanup the `os` module.
    debug_with_pdb = bool(os.environ.pop("PYTHONDEBUGWITHPDB", None))

    del os
    del sys

    # Allow users to run the main module under pdb. Encode the call into the
    # startup script, because pdb does not support the -c argument we use to invoke
    # our startup wrapper.
    #
    # Note: use pop to avoid leaking the environment variable to the child process.
    if debug_with_pdb:
        import os
        from pdb import Pdb

        pdb = Pdb()

        if initial_commands := os.environ.pop(
            "PYTHONPDBINITIALCOMMANDS", None
        ):
            pdb.rcLines.extend(initial_commands.split("|"))

        del os

        # pyre-fixme[16]: Module `runpy` has no attribute `_run_module_as_main`.
        pdb.runcall(runpy._run_module_as_main, module, False)

    else:
        # pyre-fixme[16]: Module `runpy` has no attribute `_run_module_as_main`.
        runpy._run_module_as_main(module, False)


__invoke_main()
