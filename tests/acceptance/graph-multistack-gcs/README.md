Three stacks laid out the way a self-managed GCS backend keeps one state
prefix per stack directory (`gs://state-bucket/<dir>`), each deployed with
project = stack = its directory name. A consumer's same-bucket guess is wrong
for every reference here; `reference_backends` gives the exact location, and
`resolve_cross_stack` joins each consumer to the exporting resource. Not
deployed by `run_acceptance.sh` (no top-level `Pulumi.yaml`).
