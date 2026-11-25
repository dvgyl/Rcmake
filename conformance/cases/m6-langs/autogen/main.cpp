#include "widget.h"
#include <QApplication>
#include <QFile>

int main(int argc, char **argv)
{
    QApplication app(argc, argv);
    Widget widget;
    QFile resource(":/icons/message.txt");
    return widget.labelCount() == 1 && resource.open(QIODevice::ReadOnly)
        && resource.readAll().startsWith("AUTORCC") ? 0 : 1;
}
