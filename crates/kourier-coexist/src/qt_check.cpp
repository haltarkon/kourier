// QtNetwork side of the coexistence check (architecture §5.3): QSslSocket::supportsSsl() and
// one HTTPS GET through QNetworkAccessManager, which makes Qt's TLS plugin load the system
// libssl.so.3 next to the BoringSSL that libsignal-net links statically.

#include <QtCore/QByteArray>
#include <QtCore/QCoreApplication>
#include <QtCore/QEventLoop>
#include <QtCore/QTimer>
#include <QtCore/QUrl>
#include <QtNetwork/QNetworkAccessManager>
#include <QtNetwork/QNetworkProxy>
#include <QtNetwork/QNetworkReply>
#include <QtNetwork/QNetworkRequest>
#include <QtNetwork/QSslCertificate>
#include <QtNetwork/QSslConfiguration>
#include <QtNetwork/QSslSocket>

#include <algorithm>
#include <cstddef>
#include <cstring>

namespace {

void copy_string(char *out, std::size_t cap, const QString &value) {
    const QByteArray utf8 = value.toUtf8();
    const std::size_t n = std::min<std::size_t>(cap - 1, static_cast<std::size_t>(utf8.size()));
    std::memcpy(out, utf8.constData(), n);
    out[n] = '\0';
}

} // namespace

extern "C" {

// Mirrored by `QtResult` in main.rs.
struct KourierQtResult {
    int supports_ssl;
    int network_error;
    int http_status;
    long long body_len;
    char tls_backend[64];
    char ssl_library[128];
};

// Runs on the main thread. Returns 0 when TLS is supported and the GET returned 200.
int kourier_coexist_qt_https(unsigned short port, const unsigned char *ca_der, std::size_t ca_len,
                             KourierQtResult *out) {
    static int argc = 1;
    static char arg0[] = "kourier-coexist";
    static char *argv[] = {arg0, nullptr};
    if (QCoreApplication::instance() == nullptr) {
        // Lives until the process exits, like the application object of the real app.
        new QCoreApplication(argc, argv);
    }
    std::memset(out, 0, sizeof *out);
    out->supports_ssl = QSslSocket::supportsSsl() ? 1 : 0;
    copy_string(out->tls_backend, sizeof out->tls_backend, QSslSocket::activeBackend());
    copy_string(out->ssl_library, sizeof out->ssl_library, QSslSocket::sslLibraryVersionString());
    if (out->supports_ssl == 0) {
        return 1;
    }

    QSslConfiguration conf = QSslConfiguration::defaultConfiguration();
    conf.setCaCertificates(QSslCertificate::fromData(
        QByteArray(reinterpret_cast<const char *>(ca_der), static_cast<qsizetype>(ca_len)),
        QSsl::Der));

    QNetworkAccessManager nam;
    nam.setProxy(QNetworkProxy::NoProxy);
    QNetworkRequest request(QUrl(QStringLiteral("https://127.0.0.1:%1/qt").arg(port)));
    request.setSslConfiguration(conf);
    request.setTransferTimeout(10000);
    QNetworkReply *reply = nam.get(request);

    QEventLoop loop;
    QObject::connect(reply, &QNetworkReply::finished, &loop, &QEventLoop::quit);
    QTimer::singleShot(15000, &loop, &QEventLoop::quit);
    loop.exec();

    out->network_error = reply->isFinished() ? static_cast<int>(reply->error()) : -1;
    out->http_status = reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
    out->body_len = reply->readAll().size();
    delete reply;
    return (out->network_error == 0 && out->http_status == 200) ? 0 : 2;
}

} // extern "C"
