# Test CA

Test material for `tests/tls.rs`, valid until 2126. It protects nothing: the key of the server is
public on purpose, and the key of the CA was deleted after the server certificate was signed.

- `ca.pem`: the self-signed CA (`CN=Stemma test CA`, P-256), the CA file of the remote in the
  tests.
- `server.pem`, `server-key.pem`: the certificate of the responder for `localhost` and
  `127.0.0.1`, signed by the CA, and its key in PKCS#8.

The files were made with these commands:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key \
    -out ca.pem -days 36500 -subj "/CN=Stemma test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout server.key \
    -out server.csr -subj "/CN=localhost"
printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n\
keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n" > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem \
    -days 36500 -extfile ext.cnf
openssl pkcs8 -topk8 -nocrypt -in server.key -out server-key.pem
rm server.csr ext.cnf ca.srl ca.key server.key
```
