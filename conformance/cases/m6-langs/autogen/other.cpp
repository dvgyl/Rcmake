#include <QObject>

class Other final : public QObject {
    Q_OBJECT
};

int main()
{
    Other other;
    return QByteArray(other.metaObject()->className()) == "Other" ? 0 : 1;
}

#include "other.moc"
