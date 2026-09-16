# Changelog

## [0.2.0](https://github.com/BlocksOrg/blue/compare/v0.1.0...v0.2.0) (2026-09-16)


### Features

* add Blue Desktop teaser page ([#88](https://github.com/BlocksOrg/blue/issues/88)) ([e596c5d](https://github.com/BlocksOrg/blue/commit/e596c5d86254b43ee13f6cbaca19e43a2572a0b1))
* **ci:** cut dev images on demand from any ref ([#113](https://github.com/BlocksOrg/blue/issues/113)) ([80a50b1](https://github.com/BlocksOrg/blue/commit/80a50b147d6473a0c5281ae2b321cc5be703dd01))
* **ci:** manage versions with release-please ([#134](https://github.com/BlocksOrg/blue/issues/134)) ([8bece80](https://github.com/BlocksOrg/blue/commit/8bece801d104a91f4baf074a63a535084c240993))
* **ci:** publish release candidates from any ref ([#153](https://github.com/BlocksOrg/blue/issues/153)) ([5a28e5e](https://github.com/BlocksOrg/blue/commit/5a28e5e29142269c8e9db198756d1411715ce994))
* **control-api:** derive the gateway JWKS from the signing key ([#107](https://github.com/BlocksOrg/blue/issues/107)) ([1ea082d](https://github.com/BlocksOrg/blue/commit/1ea082d3b9c889c24d95a01584b9b49dd05e76e2))
* **deploy:** create the EKS cluster and make AWS datastores optional ([#72](https://github.com/BlocksOrg/blue/issues/72)) ([f0c3ba4](https://github.com/BlocksOrg/blue/commit/f0c3ba4b3143c8d62f752d50cb2934b531a48b36))
* **deploy:** front Blue with an ALB on a Route 53 zone ([#95](https://github.com/BlocksOrg/blue/issues/95)) ([34abb7f](https://github.com/BlocksOrg/blue/commit/34abb7fea9fac56f8fa5e6b70529f08e0ea8924e))
* **deploy:** let values configure Service annotations ([#86](https://github.com/BlocksOrg/blue/issues/86)) ([eba0642](https://github.com/BlocksOrg/blue/commit/eba064242d066b2523578a8c19218b80f9542450))
* **deploy:** reserve an inference proxy hostname in the AWS module ([#116](https://github.com/BlocksOrg/blue/issues/116)) ([5fbb814](https://github.com/BlocksOrg/blue/commit/5fbb814f6ba668a4d7a1012a7319eb6c718aed37))
* **proxy,deploy:** split client cert/key, and let the chart issue both ([#111](https://github.com/BlocksOrg/blue/issues/111)) ([c8a77bd](https://github.com/BlocksOrg/blue/commit/c8a77bda2ccdae7514dcc4ef5b0cfd91c89819a5))
* **website:** add blog ([#87](https://github.com/BlocksOrg/blue/issues/87)) ([8820dbf](https://github.com/BlocksOrg/blue/commit/8820dbf0eedc2caef1674efc3fb6ffac5dc7b96b))


### Bug Fixes

* **cli:** launch npm-installed agents on Windows ([#96](https://github.com/BlocksOrg/blue/issues/96)) ([28901aa](https://github.com/BlocksOrg/blue/commit/28901aa0fe90e3974f0328594b8e96a28db45b54))
* **cli:** offer installation for allowed absent agents ([#131](https://github.com/BlocksOrg/blue/issues/131)) ([b0e07d2](https://github.com/BlocksOrg/blue/commit/b0e07d2eb6ace827993fee6c5035ffa2fc58dd63))
* **cli:** offer the version repair from the agent picker ([#99](https://github.com/BlocksOrg/blue/issues/99)) ([d445642](https://github.com/BlocksOrg/blue/commit/d445642ce16ebdf7c400240359d8303c94d81909))
* **cli:** repair the detected harness installation ([#124](https://github.com/BlocksOrg/blue/issues/124)) ([ab61512](https://github.com/BlocksOrg/blue/commit/ab6151286955162439f16fb9b055679494acabbc))
* **config:** fall back to a governed default model for OpenCode gateway mode ([#123](https://github.com/BlocksOrg/blue/issues/123)) ([81a47c1](https://github.com/BlocksOrg/blue/commit/81a47c11470a7aa78704e026b95371663d07fcb4))
* **dashboard:** read sslmode with libpq semantics ([#98](https://github.com/BlocksOrg/blue/issues/98)) ([d37e02e](https://github.com/BlocksOrg/blue/commit/d37e02e2384785117e66d54e2519b008caf370e9))
* **deploy:** bump rustls to 0.23.45 for RUSTSEC-2026-0285 ([#109](https://github.com/BlocksOrg/blue/issues/109)) ([851b68d](https://github.com/BlocksOrg/blue/commit/851b68da4dfeb5449c9998a3380d4bfdbaffe50c))
* **deploy:** mount the gateway signing key into the worker ([#127](https://github.com/BlocksOrg/blue/issues/127)) ([1fbeb04](https://github.com/BlocksOrg/blue/commit/1fbeb045f7e3e89269844943d0d994cb0713c5fc)), closes [#126](https://github.com/BlocksOrg/blue/issues/126)
* **deploy:** restore the docs snapshot gate on release ([#80](https://github.com/BlocksOrg/blue/issues/80)) ([9f71cd8](https://github.com/BlocksOrg/blue/commit/9f71cd8d793174d9fdd624303c4e0de5c1e55cf9)), closes [#79](https://github.com/BlocksOrg/blue/issues/79)
* **website:** remove stale blog card top spacing ([#103](https://github.com/BlocksOrg/blue/issues/103)) ([147e2da](https://github.com/BlocksOrg/blue/commit/147e2dadc8bf8beb18af2bf502aa01e1df610f4a))


### Documentation

* correct stale README facts about the Compose stack and repo layout ([#76](https://github.com/BlocksOrg/blue/issues/76)) ([1f8bd3f](https://github.com/BlocksOrg/blue/commit/1f8bd3fa1ee83e279a8aa8fd321642fb94604a3b)), closes [#75](https://github.com/BlocksOrg/blue/issues/75)
* **docs:** quickstart as a TL;DR and Helm guide as the verified EKS walkthrough ([#125](https://github.com/BlocksOrg/blue/issues/125)) ([0478f82](https://github.com/BlocksOrg/blue/commit/0478f829a5a73dac095c7df8e9eb56fc3d110d22))
* freeze the 0.1.0 documentation snapshot ([#82](https://github.com/BlocksOrg/blue/issues/82)) ([b460c1d](https://github.com/BlocksOrg/blue/commit/b460c1d5addc5bc47d8bde627e53c2a4811543cf)), closes [#81](https://github.com/BlocksOrg/blue/issues/81)
* **website:** remove desktop capabilities heading ([#105](https://github.com/BlocksOrg/blue/issues/105)) ([ff2f428](https://github.com/BlocksOrg/blue/commit/ff2f428a358e30b978d6b21c922f6ed4ff73d7c4))


### Code Refactoring

* **dashboard:** hardcode the documentation link ([#129](https://github.com/BlocksOrg/blue/issues/129)) ([837244e](https://github.com/BlocksOrg/blue/commit/837244e071385e293eb83703bd7ef2a89e716d28))
* **website:** drop date and author from blog cards and post headers ([#102](https://github.com/BlocksOrg/blue/issues/102)) ([ac879ca](https://github.com/BlocksOrg/blue/commit/ac879cab8f51d1e58ebd1d265a7ad96d4908e424))
