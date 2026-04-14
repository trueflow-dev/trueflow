# Website

Static website content for `trueflow.dev`.

## Local preview

From the repo root:

```sh
python3 -m http.server --directory website 8080
```

Then open <http://localhost:8080>.

## Files

- `index.html` — landing page
- `about/index.html` — about page
- `install/index.html` — human install page
- `install.sh` — one-line installer entrypoint
- `site.css` — shared page styles
- `assets/` — self-contained website assets for deploy

## URL contract

The site and release flow use one domain:

- `/` — landing page
- `/about/` — about page
- `/install/` — human install instructions
- `/install.sh` — shell installer
- `/download/<artifact_name>` — raw release artifacts and checksums

Current binary scope is Apple Silicon macOS and Linux x86_64.

## Deployment shape

Planned AWS hosting:

- `trueflow.dev` — canonical site host
- `www.trueflow.dev` — redirect to `trueflow.dev`
- private S3 bucket behind CloudFront OAC

AWS infra is defined separately under `infra/terraform/`.
